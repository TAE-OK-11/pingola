use std::net::TcpStream;

use anyhow::{Context, Result, anyhow, bail};
use boring::ssl::{SslConnector, SslMethod, SslVerifyMode, SslVersion};

const HYBRID_GROUP: &str = "X25519MLKEM768";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let address = args
        .next()
        .ok_or_else(|| anyhow!("usage: pq_tls_probe <address> <server-name>"))?;
    let server_name = args.next().ok_or_else(|| anyhow!("missing server name"))?;
    if args.next().is_some() {
        bail!("too many arguments");
    }

    let mut builder = SslConnector::builder(SslMethod::tls())?;
    builder.set_verify(SslVerifyMode::NONE);
    builder.set_min_proto_version(Some(SslVersion::TLS1_3))?;
    builder.set_max_proto_version(Some(SslVersion::TLS1_3))?;
    builder.set_curves_list(HYBRID_GROUP)?;
    let connector = builder.build();

    let tcp = TcpStream::connect(&address)
        .with_context(|| format!("failed to connect TCP TLS probe to {address}"))?;
    let stream = connector
        .connect(&server_name, tcp)
        .map_err(|error| anyhow!("hybrid PQ TLS handshake failed: {error}"))?;
    let negotiated = stream.ssl().curve_name().unwrap_or("unknown");
    if negotiated != HYBRID_GROUP {
        bail!("expected {HYBRID_GROUP}, negotiated {negotiated}");
    }
    println!("{negotiated}");
    Ok(())
}
