use std::{mem, net::SocketAddr, pin::Pin, task::Context};

use futures::{ready, Future, Poll, Stream};
use http::{response, HeaderMap, Request, Response};
use quinn::{EndpointBuilder, EndpointDriver, EndpointError, RecvStream, SendStream};
use quinn_proto::{Side, StreamId};

use crate::{
    body::{Body, BodyReader, BodyWriter},
    connection::{ConnectionDriver, ConnectionRef},
    frame::{FrameDecoder, FrameStream, WriteFrame},
    headers::{DecodeHeaders, SendHeaders},
    proto::{
        frame::{DataFrame, HttpFrame},
        headers::Header,
        ErrorCode,
    },
    streams::Reset,
    try_take, Error, Settings,
};

pub struct Builder {
    endpoint: EndpointBuilder,
    settings: Settings,
}

impl Builder {
    pub fn new(endpoint: EndpointBuilder) -> Self {
        Self {
            endpoint,
            settings: Settings::default(),
        }
    }

    pub fn settings(&mut self, settings: Settings) -> &mut Self {
        self.settings = settings;
        self
    }

    pub fn bind(
        self,
        addr: &SocketAddr,
    ) -> Result<(EndpointDriver, Server, IncomingConnection), EndpointError> {
        let (endpoint_driver, _endpoint, incoming) = self.endpoint.bind(addr)?;
        Ok((
            endpoint_driver,
            Server,
            IncomingConnection {
                incoming,
                settings: self.settings.clone(),
            },
        ))
    }
}

pub struct Server;

pub struct IncomingConnection {
    incoming: quinn::Incoming,
    settings: Settings,
}

impl Stream for IncomingConnection {
    type Item = Connecting;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        Poll::Ready(
            ready!(Pin::new(&mut self.incoming).poll_next(cx)).map(|c| Connecting {
                connecting: c,
                settings: self.settings.clone(),
            }),
        )
    }
}

pub struct Connecting {
    connecting: quinn::Connecting,
    settings: Settings,
}

impl Future for Connecting {
    type Output = Result<(quinn::ConnectionDriver, ConnectionDriver, IncomingRequest), Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let quinn::NewConnection {
            driver,
            connection,
            bi_streams,
            uni_streams,
            ..
        } = ready!(Pin::new(&mut self.connecting).poll(cx))?;
        let conn_ref = ConnectionRef::new(
            connection,
            Side::Server,
            uni_streams,
            bi_streams,
            self.settings.clone(),
        )?;
        Poll::Ready(Ok((
            driver,
            ConnectionDriver(conn_ref.clone()),
            IncomingRequest(conn_ref),
        )))
    }
}

pub struct IncomingRequest(ConnectionRef);

impl Stream for IncomingRequest {
    type Item = RecvRequest;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let (send, recv) = {
            let conn = &mut self.0.h3.lock().unwrap();
            match conn.requests.pop_front() {
                Some(s) => s,
                None => {
                    conn.requests_task = Some(cx.waker().clone());
                    return Poll::Pending;
                }
            }
        };
        Poll::Ready(Some(RecvRequest::new(recv, send, self.0.clone())))
    }
}

enum RecvRequestState {
    Receiving(FrameStream, SendStream),
    Decoding(DecodeHeaders),
    Finished,
}

pub struct RecvRequest {
    state: RecvRequestState,
    conn: ConnectionRef,
    stream_id: StreamId,
    streams: Option<(FrameStream, SendStream)>,
}

impl RecvRequest {
    fn new(recv: RecvStream, send: SendStream, conn: ConnectionRef) -> Self {
        Self {
            conn,
            stream_id: send.id(),
            streams: None,
            state: RecvRequestState::Receiving(FrameDecoder::stream(recv), send),
        }
    }

    fn build_request(&self, headers: Header) -> Result<Request<()>, Error> {
        let (method, uri, headers) = headers.into_request_parts()?;
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .version(http::version::Version::HTTP_3)
            .body(())
            .unwrap();
        *request.headers_mut() = headers;
        Ok(request)
    }

    pub fn reject(mut self) {
        let state = mem::replace(&mut self.state, RecvRequestState::Finished);
        if let RecvRequestState::Receiving(recv, mut send) = state {
            recv.reset(ErrorCode::REQUEST_REJECTED);
            send.reset(ErrorCode::REQUEST_REJECTED.into());
        }
    }
}

impl Future for RecvRequest {
    type Output = Result<(Request<()>, BodyReader, Sender), Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        loop {
            match self.state {
                RecvRequestState::Receiving(ref mut frames, _) => {
                    match ready!(Pin::new(frames).poll_next(cx)) {
                        None => return Poll::Ready(Err(Error::peer("received an empty request"))),
                        Some(Ok(HttpFrame::Headers(f))) => {
                            let decode = DecodeHeaders::new(f, self.conn.clone(), self.stream_id);
                            match mem::replace(&mut self.state, RecvRequestState::Decoding(decode))
                            {
                                RecvRequestState::Receiving(f, s) => self.streams = Some((f, s)),
                                _ => unreachable!("Invalid state"),
                            }
                        }
                        Some(x) => {
                            let (code, error) = match x {
                                Err(e) => (e.code(), e.into()),
                                Ok(_) => (
                                    ErrorCode::FRAME_UNEXPECTED,
                                    Error::peer("first frame is not headers"),
                                ),
                            };
                            match mem::replace(&mut self.state, RecvRequestState::Finished) {
                                RecvRequestState::Receiving(recv, _) => recv.reset(code),
                                _ => unreachable!(),
                            }
                            return Poll::Ready(Err(error));
                        }
                    }
                }
                RecvRequestState::Decoding(ref mut decode) => {
                    let header = ready!(Pin::new(decode).poll(cx))?;
                    self.state = RecvRequestState::Finished;
                    let (recv, send) = try_take(&mut self.streams, "Recv request invalid state")?;
                    return Poll::Ready(Ok((
                        self.build_request(header)?,
                        BodyReader::new(recv, self.conn.clone(), self.stream_id, false),
                        Sender {
                            send,
                            stream_id: self.stream_id,
                            conn: self.conn.clone(),
                        },
                    )));
                }
                RecvRequestState::Finished => {
                    return Poll::Ready(Err(Error::peer("polled after ready")));
                }
            };
        }
    }
}

pub struct Sender {
    send: SendStream,
    conn: ConnectionRef,
    stream_id: StreamId,
}

impl Sender {
    pub async fn send_response<T: Into<Body>>(
        self,
        response: Response<T>,
    ) -> Result<BodyWriter, Error> {
        let (
            response::Parts {
                status, headers, ..
            },
            body,
        ) = response.into_parts();

        let send = SendHeaders::new(
            Header::response(status, headers),
            &self.conn,
            self.send,
            self.stream_id,
        )?
        .await?;
        let send = match body.into() {
            Body::None => send,
            Body::Buf(payload) => WriteFrame::new(send, DataFrame { payload }).await?,
        };
        Ok(BodyWriter::new(send, self.conn, self.stream_id, None, true))
    }

    pub fn cancel(mut self) {
        self.send.reset(ErrorCode::REQUEST_REJECTED.into());
    }
}
