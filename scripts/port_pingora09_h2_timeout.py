from pathlib import Path

server = Path("vendor/pingora-core-0.9.0/src/protocols/http/v2/server.rs")
s = server.read_text()

old = """    // buffered request body for retry logic
    retry_buffer: Option<FixedBuffer>,
    // digest to record underlying connection info
    digest: Arc<Digest>,"""
new = """    // buffered request body for retry logic
    retry_buffer: Option<FixedBuffer>,
    /// The read timeout applied to each downstream request-body read.
    read_timeout: Option<Duration>,
    // digest to record underlying connection info
    digest: Arc<Digest>,"""
assert old in s, "HttpSession field insertion point not found"
s = s.replace(old, new, 1)

old = """            retry_buffer: None,
            digest,
            write_timeout: None,"""
new = """            retry_buffer: None,
            read_timeout: None,
            digest,
            write_timeout: None,"""
assert old in s, "HttpSession initializer insertion point not found"
s = s.replace(old, new, 1)

old = """    /// Read request body bytes. `None` when there is no more body to read.
    pub async fn read_body_bytes(&mut self) -> Result<Option<Bytes>> {
        // TODO: timeout
        let data = self.request_body_reader.data().await.transpose().or_err(
            ErrorType::ReadError,
            "while reading downstream request body",
        )?;"""
new = """    /// Read request body bytes. `None` when there is no more body to read.
    pub async fn read_body_bytes(&mut self) -> Result<Option<Bytes>> {
        let read_timeout = self.read_timeout;
        let read = async {
            self.request_body_reader.data().await.transpose().or_err(
                ErrorType::ReadError,
                "while reading downstream request body",
            )
        };
        let data = match read_timeout {
            Some(deadline) => match timeout(deadline, read).await {
                Ok(result) => result?,
                Err(_) => {
                    return Error::e_explain(
                        ErrorType::ReadTimedout,
                        format!("reading downstream H2 body, timeout: {deadline:?}"),
                    )
                }
            },
            None => read.await?,
        };"""
assert old in s, "read_body_bytes implementation not found"
s = s.replace(old, new, 1)

marker = """        Ok(data)
    }

    #[doc(hidden)]
    pub fn poll_read_body_bytes("""
replacement = """        Ok(data)
    }

    /// Sets the downstream request-body read timeout. The timeout is reset for each read.
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    /// Get the downstream request-body read timeout.
    pub fn get_read_timeout(&self) -> Option<Duration> {
        self.read_timeout
    }

    #[doc(hidden)]
    pub fn poll_read_body_bytes("""
assert marker in s, "read timeout methods insertion point not found"
s = s.replace(marker, replacement, 1)
server.write_text(s)
