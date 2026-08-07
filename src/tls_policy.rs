use boring::ssl::{SslContextBuilder, SslMethod, SslVersion};

pub const HYBRID_PQ_GROUPS: &str = "X25519MLKEM768:X25519:P-256";
pub fn new_hybrid_pq_context() -> Result<SslContextBuilder, boring::error::ErrorStack> {
    let mut builder = SslContextBuilder::new(SslMethod::tls())?;
    builder.set_min_proto_version(Some(SslVersion::TLS1_3))?;
    builder.set_max_proto_version(Some(SslVersion::TLS1_3))?;
    builder.set_curves_list(HYBRID_PQ_GROUPS)?;
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloudflare_boringssl_exposes_x25519_mlkem768() {
        let mut builder = SslContextBuilder::new(SslMethod::tls()).unwrap();
        builder.set_curves_list("X25519MLKEM768").unwrap();
    }
}
