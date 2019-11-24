use std::{fs, io, path::PathBuf};
use structopt::{self, StructOpt};

use anyhow::{anyhow, Result};
use futures::AsyncReadExt;
use http::{method::Method, Request};
use tracing::{error, info};
use url::Url;

use quinn_h3::{
    self,
    client::{Builder as ClientBuilder, Client},
};

#[derive(StructOpt, Debug, Clone)]
#[structopt(name = "h3_client")]
struct Opt {
    #[structopt(default_value = "http://127.0.0.1:4433/Cargo.toml")]
    url: Url,

    /// Custom certificate authority to trust, in DER format
    #[structopt(parse(from_os_str), long = "ca")]
    ca: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .finish(),
    )
    .unwrap();
    let options = Opt::from_args();

    let mut client = ClientBuilder::default();
    if let Some(ca_path) = options.ca {
        client.add_certificate_authority(quinn::Certificate::from_der(&fs::read(&ca_path)?)?)?;
    } else {
        let dirs = directories::ProjectDirs::from("org", "quinn", "quinn-examples").unwrap();
        match fs::read(dirs.data_local_dir().join("cert.der")) {
            Ok(cert) => {
                client.add_certificate_authority(quinn::Certificate::from_der(&cert)?)?;
            }
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => {
                info!("local server certificate not found");
            }
            Err(e) => {
                error!("failed to open local server certificate: {}", e);
            }
        }
    }

    let (endpoint_driver, client) = client.build()?;
    tokio::spawn(async move {
        if let Err(e) = endpoint_driver.await {
            eprintln!("quic driver error: {}", e)
        }
    });

    match request(client, &options.url).await {
        Ok(_) => println!("client finished"),
        Err(e) => println!("client failed: {:?}", e),
    }

    Ok(())
}

async fn request(client: Client, url: &Url) -> Result<()> {
    let (quic_driver, h3_driver, conn) = client
        .connect(url)?
        .await
        .map_err(|e| anyhow!("failed ot connect: {:?}", e))?;

    tokio::spawn(async move {
        if let Err(e) = h3_driver.await {
            eprintln!("h3 client error: {}", e)
        }
    });

    tokio::spawn(async move {
        if let Err(e) = quic_driver.await {
            eprintln!("h3 client error: {}", e)
        }
    });

    let request = Request::builder()
        .method(Method::GET)
        .uri(url.path())
        .header("client", "quinn-h3:0.0.1")
        .body(())
        .expect("failed to build request");

    let (recv_response, _) = conn.send_request(request).await?;
    let (response, mut recv_body) = recv_response.await?;

    println!("received response: {:?}", response);

    let mut body = Vec::with_capacity(1024);
    recv_body.read_to_end(&mut body).await?;

    println!("received body: {}", String::from_utf8_lossy(&body));

    if let Some(trailers) = recv_body.trailers().await {
        println!("received trailers: {:?}", trailers);
    }
    conn.close();

    Ok(())
}
