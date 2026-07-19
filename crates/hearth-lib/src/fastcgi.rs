use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::timeout as tokio_timeout;

const FCGI_VERSION_1: u8 = 1;
const FCGI_BEGIN_REQUEST: u8 = 1;
const FCGI_END_REQUEST: u8 = 3;
const FCGI_PARAMS: u8 = 4;
const FCGI_STDIN: u8 = 5;
const FCGI_STDOUT: u8 = 6;
const FCGI_STDERR: u8 = 7;
const FCGI_RESPONDER: u16 = 1;
const FCGI_REQUEST_COMPLETE: u8 = 0;
const REQUEST_ID: u16 = 1;

const MAX_PARAMS_BYTES: usize = 16 * 1024;
const MAX_RECORD_CONTENT: usize = u16::MAX as usize;
const MAX_INBOUND_BYTES: usize = 256 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_CGI_HEADER_BYTES: usize = 8 * 1024;
const MAX_CGI_HEADERS: usize = 32;
const MAX_STDERR_EXCERPT: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FcgiResponse {
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub stderr_len: usize,
}

pub async fn request(
    socket: &Path,
    params: &[(String, String)],
    timeout: Duration,
) -> Result<FcgiResponse, String> {
    let request_bytes = build_request(params)?;
    let phase = Phase::new();
    let operation_phase = phase.clone();
    match tokio_timeout(timeout, async move {
        request_inner(socket, &request_bytes, &operation_phase).await
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(format!(
            "FastCGI timeout during {} after {} ms",
            phase.label(),
            timeout.as_millis()
        )),
    }
}

#[derive(Clone)]
struct Phase(Arc<AtomicU8>);

impl Phase {
    fn new() -> Self {
        Self(Arc::new(AtomicU8::new(0)))
    }

    fn set(&self, phase: u8) {
        self.0.store(phase, Ordering::Relaxed);
    }

    fn label(&self) -> &'static str {
        match self.0.load(Ordering::Relaxed) {
            0 => "connect",
            1 => "request write",
            2 => "response header",
            3 => "response content",
            4 => "response padding",
            5 => "clean EOF after END_REQUEST",
            _ => "response parsing",
        }
    }
}

#[derive(Debug)]
struct Record {
    record_type: u8,
    content: Vec<u8>,
}

async fn request_inner(
    socket: &Path,
    request_bytes: &[u8],
    phase: &Phase,
) -> Result<FcgiResponse, String> {
    phase.set(0);
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|error| format!("FastCGI connect failed: {error}"))?;

    phase.set(1);
    stream
        .write_all(request_bytes)
        .await
        .map_err(|error| format!("FastCGI request write failed: {error}"))?;
    stream
        .flush()
        .await
        .map_err(|error| format!("FastCGI request flush failed: {error}"))?;

    let mut total_inbound = 0_usize;
    let mut stdout = Vec::new();
    let mut stdout_terminated = false;
    let mut stdout_records = 0_usize;
    let mut saw_nonempty_stdout = false;
    let mut stderr_len = 0_usize;
    let mut stderr_excerpt = Vec::new();
    let mut stderr_terminated = false;

    loop {
        let record = read_record(&mut stream, &mut total_inbound, phase)
            .await?
            .ok_or_else(|| "FastCGI rejection: EOF before END_REQUEST".to_string())?;

        match record.record_type {
            FCGI_STDOUT => {
                stdout_records = stdout_records.saturating_add(1);
                if stdout_terminated {
                    return Err("FastCGI rejection: STDOUT record after terminator".to_string());
                }
                if record.content.is_empty() {
                    stdout_terminated = true;
                } else {
                    saw_nonempty_stdout = true;
                    stdout.extend_from_slice(&record.content);
                }
            }
            FCGI_STDERR => {
                if stderr_terminated {
                    return Err("FastCGI rejection: STDERR record after terminator".to_string());
                }
                if record.content.is_empty() {
                    stderr_terminated = true;
                } else {
                    stderr_len = stderr_len
                        .checked_add(record.content.len())
                        .ok_or_else(|| "FastCGI rejection: STDERR length overflow".to_string())?;
                    let remaining = MAX_STDERR_EXCERPT.saturating_sub(stderr_excerpt.len());
                    stderr_excerpt
                        .extend_from_slice(&record.content[..record.content.len().min(remaining)]);
                }
            }
            FCGI_END_REQUEST => {
                validate_end_request(&record.content)?;
                break;
            }
            _ => unreachable!("record type was validated while reading"),
        }
    }

    require_clean_eof(&mut stream, &mut total_inbound, phase).await?;

    if stderr_len != 0 {
        return Err(format!(
            "FastCGI rejection: non-empty STDERR ({stderr_len} bytes): {}",
            sanitize_excerpt(&stderr_excerpt)
        ));
    }
    if !stdout_terminated && !saw_nonempty_stdout {
        return Err(format!(
            "FastCGI rejection: END_REQUEST before any nonempty STDOUT or explicit empty terminator ({} bytes in {stdout_records} records)",
            stdout.len()
        ));
    }

    phase.set(6);
    parse_cgi_response(stdout, stderr_len)
}

fn build_request(params: &[(String, String)]) -> Result<Vec<u8>, String> {
    let params = encode_params(params)?;
    if params.len() > MAX_PARAMS_BYTES {
        return Err(format!(
            "FastCGI request rejected: encoded PARAMS are {} bytes; limit is {MAX_PARAMS_BYTES}",
            params.len()
        ));
    }

    let mut bytes = Vec::with_capacity(params.len() + 40);
    append_record(
        &mut bytes,
        FCGI_BEGIN_REQUEST,
        REQUEST_ID,
        &[0, FCGI_RESPONDER as u8, 0, 0, 0, 0, 0, 0],
    );
    append_stream_records(&mut bytes, FCGI_PARAMS, REQUEST_ID, &params);
    append_record(&mut bytes, FCGI_PARAMS, REQUEST_ID, &[]);
    append_record(&mut bytes, FCGI_STDIN, REQUEST_ID, &[]);
    Ok(bytes)
}

fn encode_params(params: &[(String, String)]) -> Result<Vec<u8>, String> {
    let mut encoded = Vec::new();
    for (name, value) in params {
        append_length(&mut encoded, name.len())?;
        append_length(&mut encoded, value.len())?;
        encoded.extend_from_slice(name.as_bytes());
        encoded.extend_from_slice(value.as_bytes());
        if encoded.len() > MAX_PARAMS_BYTES {
            return Err(format!(
                "FastCGI request rejected: encoded PARAMS exceed {MAX_PARAMS_BYTES} bytes"
            ));
        }
    }
    Ok(encoded)
}

fn append_length(target: &mut Vec<u8>, length: usize) -> Result<(), String> {
    if length < 128 {
        target.push(length as u8);
        return Ok(());
    }
    let length = u32::try_from(length)
        .ok()
        .filter(|length| *length <= 0x7fff_ffff)
        .ok_or_else(|| "FastCGI request rejected: PARAMS name/value length overflow".to_string())?;
    target.extend_from_slice(&(length | 0x8000_0000).to_be_bytes());
    Ok(())
}

fn append_stream_records(target: &mut Vec<u8>, record_type: u8, request_id: u16, body: &[u8]) {
    for chunk in body.chunks(MAX_RECORD_CONTENT) {
        append_record(target, record_type, request_id, chunk);
    }
}

fn append_record(target: &mut Vec<u8>, record_type: u8, request_id: u16, body: &[u8]) {
    debug_assert!(body.len() <= MAX_RECORD_CONTENT);
    target.extend_from_slice(&[
        FCGI_VERSION_1,
        record_type,
        (request_id >> 8) as u8,
        request_id as u8,
        (body.len() >> 8) as u8,
        body.len() as u8,
        0,
        0,
    ]);
    target.extend_from_slice(body);
}

async fn read_record(
    stream: &mut UnixStream,
    total_inbound: &mut usize,
    phase: &Phase,
) -> Result<Option<Record>, String> {
    phase.set(2);
    let mut header = [0_u8; 8];
    let first = stream
        .read(&mut header[..1])
        .await
        .map_err(|error| format!("FastCGI response header read failed: {error}"))?;
    if first == 0 {
        return Ok(None);
    }
    stream
        .read_exact(&mut header[1..])
        .await
        .map_err(|error| format!("FastCGI rejection: truncated 8-byte header: {error}"))?;

    if header[0] != FCGI_VERSION_1 {
        return Err(format!(
            "FastCGI rejection: unsupported version {}",
            header[0]
        ));
    }
    if header[7] != 0 {
        return Err("FastCGI rejection: non-zero reserved header byte".to_string());
    }

    let request_id = u16::from_be_bytes([header[2], header[3]]);
    if request_id == 0 {
        return Err("FastCGI rejection: management record requestId 0".to_string());
    }
    if request_id != REQUEST_ID {
        return Err(format!(
            "FastCGI rejection: foreign requestId {request_id}; expected {REQUEST_ID}"
        ));
    }
    if !matches!(header[1], FCGI_STDOUT | FCGI_STDERR | FCGI_END_REQUEST) {
        return Err(format!(
            "FastCGI rejection: unexpected record type {}",
            header[1]
        ));
    }

    let content_len = u16::from_be_bytes([header[4], header[5]]) as usize;
    let padding_len = header[6] as usize;
    let record_len = 8_usize
        .checked_add(content_len)
        .and_then(|length| length.checked_add(padding_len))
        .ok_or_else(|| "FastCGI rejection: inbound length overflow".to_string())?;
    *total_inbound = total_inbound
        .checked_add(record_len)
        .ok_or_else(|| "FastCGI rejection: inbound length overflow".to_string())?;
    if *total_inbound > MAX_INBOUND_BYTES {
        return Err(format!(
            "FastCGI rejection: inbound response exceeds {MAX_INBOUND_BYTES} bytes"
        ));
    }

    phase.set(3);
    let mut content = vec![0_u8; content_len];
    stream
        .read_exact(&mut content)
        .await
        .map_err(|error| format!("FastCGI rejection: truncated record content: {error}"))?;

    phase.set(4);
    let mut padding = vec![0_u8; padding_len];
    stream
        .read_exact(&mut padding)
        .await
        .map_err(|error| format!("FastCGI rejection: truncated record padding: {error}"))?;

    Ok(Some(Record {
        record_type: header[1],
        content,
    }))
}

fn validate_end_request(body: &[u8]) -> Result<(), String> {
    if body.len() != 8 {
        return Err(format!(
            "FastCGI rejection: END_REQUEST body is {} bytes; expected 8",
            body.len()
        ));
    }
    let app_status = u32::from_be_bytes(body[..4].try_into().expect("four bytes"));
    if app_status != 0 {
        return Err(format!(
            "FastCGI rejection: END_REQUEST appStatus is {app_status}; expected 0"
        ));
    }
    if body[4] != FCGI_REQUEST_COMPLETE {
        return Err(format!(
            "FastCGI rejection: END_REQUEST protocolStatus is {}; expected COMPLETE",
            body[4]
        ));
    }
    if body[5..] != [0, 0, 0] {
        return Err("FastCGI rejection: END_REQUEST reserved bytes are non-zero".to_string());
    }
    Ok(())
}

async fn require_clean_eof(
    stream: &mut UnixStream,
    total_inbound: &mut usize,
    phase: &Phase,
) -> Result<(), String> {
    phase.set(5);
    let mut byte = [0_u8; 1];
    match stream.read(&mut byte).await {
        Ok(0) => Ok(()),
        Ok(count) => {
            *total_inbound = total_inbound.saturating_add(count);
            if *total_inbound > MAX_INBOUND_BYTES {
                Err(format!(
                    "FastCGI rejection: inbound response exceeds {MAX_INBOUND_BYTES} bytes"
                ))
            } else {
                Err("FastCGI rejection: bytes or duplicate record after END_REQUEST".to_string())
            }
        }
        Err(error) => Err(format!(
            "FastCGI rejection: clean EOF check after END_REQUEST failed: {error}"
        )),
    }
}

fn parse_cgi_response(stdout: Vec<u8>, stderr_len: usize) -> Result<FcgiResponse, String> {
    let split = stdout
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "FastCGI rejection: CGI response lacks CRLFCRLF separator".to_string())?;
    let header_bytes = split + 4;
    if header_bytes > MAX_CGI_HEADER_BYTES {
        return Err(format!(
            "FastCGI rejection: CGI headers exceed {MAX_CGI_HEADER_BYTES} bytes"
        ));
    }
    let body = stdout[header_bytes..].to_vec();
    if body.len() > MAX_BODY_BYTES {
        return Err(format!(
            "FastCGI rejection: CGI body exceeds {MAX_BODY_BYTES} bytes"
        ));
    }

    let text = std::str::from_utf8(&stdout[..split])
        .map_err(|_| "FastCGI rejection: CGI headers are not valid UTF-8".to_string())?;
    let mut headers = Vec::new();
    let mut names = HashSet::new();
    let mut status = 200_u16;
    if !text.is_empty() {
        for line in text.split("\r\n") {
            if headers.len() == MAX_CGI_HEADERS {
                return Err(format!(
                    "FastCGI rejection: CGI response has more than {MAX_CGI_HEADERS} headers"
                ));
            }
            let (name, value) = line.split_once(':').ok_or_else(|| {
                "FastCGI rejection: malformed CGI header without colon".to_string()
            })?;
            if !valid_header_name(name) {
                return Err("FastCGI rejection: malformed CGI header name".to_string());
            }
            let value = value.trim_matches([' ', '\t']);
            if value
                .bytes()
                .any(|byte| byte < 0x20 && byte != b'\t' || byte == 0x7f)
            {
                return Err("FastCGI rejection: malformed CGI header value".to_string());
            }
            let folded = name.to_ascii_lowercase();
            if !names.insert(folded.clone()) {
                return Err(format!("FastCGI rejection: duplicate CGI header {name}"));
            }
            if folded == "status" {
                let code = value.split_ascii_whitespace().next().ok_or_else(|| {
                    "FastCGI rejection: malformed empty Status header".to_string()
                })?;
                if code.len() != 3 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err("FastCGI rejection: malformed Status header".to_string());
                }
                status = code
                    .parse()
                    .map_err(|_| "FastCGI rejection: malformed Status header".to_string())?;
            }
            headers.push((name.to_string(), value.to_string()));
        }
    }
    if !(200..300).contains(&status) {
        return Err(format!(
            "FastCGI rejection: CGI Status {status} is not successful"
        ));
    }

    Ok(FcgiResponse {
        headers,
        body,
        stderr_len,
    })
}

fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn sanitize_excerpt(bytes: &[u8]) -> String {
    let mut excerpt = String::new();
    for &byte in bytes {
        let fragment = match byte {
            b' '..=b'~' => (byte as char).to_string(),
            b'\n' => "\\n".to_string(),
            b'\r' => "\\r".to_string(),
            b'\t' => "\\t".to_string(),
            _ => format!("\\x{byte:02x}"),
        };
        if excerpt.len() + fragment.len() > MAX_STDERR_EXCERPT {
            break;
        }
        excerpt.push_str(&fragment);
    }
    excerpt
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    fn record(record_type: u8, request_id: u16, content: &[u8], padding: u8) -> Vec<u8> {
        let mut bytes = vec![
            1,
            record_type,
            (request_id >> 8) as u8,
            request_id as u8,
            (content.len() >> 8) as u8,
            content.len() as u8,
            padding,
            0,
        ];
        bytes.extend_from_slice(content);
        bytes.extend(std::iter::repeat_n(0xa5, padding as usize));
        bytes
    }

    fn end_request(app_status: u32, protocol_status: u8) -> Vec<u8> {
        let mut body = app_status.to_be_bytes().to_vec();
        body.extend_from_slice(&[protocol_status, 0, 0, 0]);
        record(3, 1, &body, 0)
    }

    fn valid_response(body: &[u8]) -> Vec<u8> {
        let mut response = record(
            6,
            1,
            b"Content-Type: text/plain\r\nStatus: 200 OK\r\n\r\n",
            0,
        );
        response.extend(record(6, 1, body, 0));
        response.extend(record(6, 1, b"", 0));
        response.extend(end_request(0, 0));
        response
    }

    async fn read_request(stream: &mut tokio::net::UnixStream) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut saw_empty_stdin = false;
        while !saw_empty_stdin {
            let mut header = [0_u8; 8];
            stream.read_exact(&mut header).await.unwrap();
            let content_len = u16::from_be_bytes([header[4], header[5]]) as usize;
            let padding_len = header[6] as usize;
            let mut tail = vec![0_u8; content_len + padding_len];
            stream.read_exact(&mut tail).await.unwrap();
            bytes.extend_from_slice(&header);
            bytes.extend_from_slice(&tail);
            saw_empty_stdin = header[1] == 5 && content_len == 0;
        }
        bytes
    }

    async fn scripted_request(
        response: Vec<u8>,
        params: &[(String, String)],
    ) -> Result<FcgiResponse, String> {
        let fixture = tempfile::Builder::new()
            .prefix("hearth-fcgi-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let socket = fixture.path().join("f.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            let _ = stream.write_all(&response).await;
            let _ = stream.shutdown().await;
        });
        let result = request(&socket, params, Duration::from_secs(1)).await;
        if result.is_ok() {
            server.await.unwrap();
        } else {
            server.abort();
            let _ = server.await;
        }
        result
    }

    async fn assert_rejected(response: Vec<u8>, expected: &str) -> String {
        let error = scripted_request(response, &[]).await.unwrap_err();
        assert!(
            error.contains(expected),
            "expected {expected:?} in rejection: {error}"
        );
        error
    }

    #[tokio::test]
    async fn responder_round_trip_reassembles_stdout_and_parses_cgi_headers() {
        let result = scripted_request(
            valid_response(br#"{"hearth_probe":1}"#),
            &[("REQUEST_METHOD".into(), "GET".into())],
        )
        .await
        .unwrap();

        assert_eq!(result.body, br#"{"hearth_probe":1}"#);
        assert_eq!(result.headers.len(), 2);
        assert_eq!(result.stderr_len, 0);
    }

    #[tokio::test]
    async fn rejects_invalid_record_headers_and_truncation() {
        let mut wrong_version = valid_response(b"ok");
        wrong_version[0] = 2;
        assert_rejected(wrong_version, "unsupported version").await;

        let mut management = valid_response(b"ok");
        management[2] = 0;
        management[3] = 0;
        assert_rejected(management, "management record").await;

        let mut foreign = valid_response(b"ok");
        foreign[3] = 2;
        assert_rejected(foreign, "foreign requestId").await;

        let mut unknown = valid_response(b"ok");
        unknown[1] = 2;
        assert_rejected(unknown, "unexpected record type").await;

        let mut reserved = valid_response(b"ok");
        reserved[7] = 1;
        assert_rejected(reserved, "reserved header").await;

        assert_rejected(vec![1, 6, 0, 1], "truncated 8-byte header").await;

        let mut truncated_content = record(6, 1, b"hello", 0);
        truncated_content.truncate(truncated_content.len() - 2);
        assert_rejected(truncated_content, "truncated record content").await;

        let mut truncated_padding = record(6, 1, b"x", 3);
        truncated_padding.truncate(truncated_padding.len() - 1);
        assert_rejected(truncated_padding, "truncated record padding").await;
    }

    #[tokio::test]
    async fn rejects_missing_duplicate_or_post_end_records() {
        assert_rejected(record(6, 1, b"", 0), "EOF before END_REQUEST").await;

        let mut duplicate = valid_response(b"ok");
        duplicate.extend(end_request(0, 0));
        assert_rejected(duplicate, "after END_REQUEST").await;

        let mut trailing = valid_response(b"ok");
        trailing.push(0xff);
        assert_rejected(trailing, "after END_REQUEST").await;

        assert_rejected(
            end_request(0, 0),
            "before any nonempty STDOUT or explicit empty terminator",
        )
        .await;

        let mut only_empty_stdout = record(6, 1, b"", 0);
        only_empty_stdout.extend(end_request(0, 0));
        assert_rejected(only_empty_stdout, "lacks CRLFCRLF").await;
    }

    #[tokio::test]
    async fn php_fpm_implicit_stdout_termination_is_accepted_only_after_nonempty_output() {
        let mut response = record(
            6,
            1,
            b"Content-Type: text/plain\r\nStatus: 200 OK\r\n\r\nphp-fpm",
            0,
        );
        response.extend(end_request(0, 0));

        let result = scripted_request(response, &[]).await.unwrap();
        assert_eq!(result.body, b"php-fpm");
        assert_eq!(result.stderr_len, 0);
    }

    #[tokio::test]
    async fn implicit_termination_rejects_stderr_status_cgi_and_post_end_violations() {
        let mut stderr = record(6, 1, b"Content-Type: text/plain\r\n\r\nok", 0);
        stderr.extend(record(7, 1, b"worker warning", 0));
        stderr.extend(end_request(0, 0));
        assert_rejected(stderr, "non-empty STDERR").await;

        let mut bad_end = record(6, 1, b"Content-Type: text/plain\r\n\r\nok", 0);
        bad_end.extend(end_request(37, 0));
        assert_rejected(bad_end, "appStatus is 37").await;

        let mut bad_status = record(6, 1, b"Status: 500 Failed\r\n\r\nno", 0);
        bad_status.extend(end_request(0, 0));
        assert_rejected(bad_status, "not successful").await;

        let mut malformed = record(6, 1, b"Missing-colon\r\n\r\nno", 0);
        malformed.extend(end_request(0, 0));
        assert_rejected(malformed, "without colon").await;

        let mut post_end = record(6, 1, b"Content-Type: text/plain\r\n\r\nok", 0);
        post_end.extend(end_request(0, 0));
        post_end.push(0xff);
        assert_rejected(post_end, "after END_REQUEST").await;
    }

    #[tokio::test]
    async fn implicit_termination_requires_clean_eof_within_whole_timeout() {
        let fixture = tempfile::tempdir_in("/private/tmp").unwrap();
        let socket = fixture.path().join("f.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            let mut response = record(6, 1, b"Content-Type: text/plain\r\n\r\nok", 0);
            response.extend(end_request(0, 0));
            stream.write_all(&response).await.unwrap();
            tokio::time::sleep(Duration::from_millis(80)).await;
        });

        let error = request(&socket, &[], Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(error.contains("clean EOF after END_REQUEST"), "{error}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_invalid_end_request_bodies_and_statuses() {
        let prefix = record(6, 1, b"Content-Type: text/plain\r\n\r\nok", 0);
        let terminator = record(6, 1, b"", 0);

        let mut wrong_len = prefix.clone();
        wrong_len.extend(&terminator);
        wrong_len.extend(record(3, 1, &[0; 7], 0));
        assert_rejected(wrong_len, "expected 8").await;

        let mut app_failed = prefix.clone();
        app_failed.extend(&terminator);
        app_failed.extend(end_request(37, 0));
        assert_rejected(app_failed, "appStatus is 37").await;

        let mut protocol_failed = prefix.clone();
        protocol_failed.extend(&terminator);
        protocol_failed.extend(end_request(0, 2));
        assert_rejected(protocol_failed, "protocolStatus").await;

        let mut reserved = prefix;
        reserved.extend(&terminator);
        let mut body = [0_u8; 8];
        body[7] = 1;
        reserved.extend(record(3, 1, &body, 0));
        assert_rejected(reserved, "reserved bytes").await;
    }

    #[tokio::test]
    async fn nonempty_stderr_discards_evidence_with_bounded_sanitized_excerpt() {
        let mut response = record(6, 1, b"Content-Type: text/plain\r\n\r\n", 0);
        let mut peer_text = vec![b'x'; 700];
        peer_text.splice(0..3, [b'\n', 0, b'\r']);
        response.extend(record(7, 1, &peer_text, 0));
        response.extend(record(6, 1, b"trusted-looking-value", 0));
        response.extend(record(7, 1, b"", 0));
        response.extend(record(6, 1, b"", 0));
        response.extend(end_request(0, 0));

        let error = assert_rejected(response, "non-empty STDERR (700 bytes)").await;
        assert!(error.contains("\\n\\x00\\r"));
        assert!(!error.contains('\n'));
        let excerpt = error.rsplit_once(": ").unwrap().1;
        assert!(excerpt.len() <= MAX_STDERR_EXCERPT);
    }

    #[tokio::test]
    async fn enforces_total_body_and_cgi_header_limits() {
        let mut inbound = Vec::new();
        for _ in 0..4 {
            inbound.extend(record(6, 1, &vec![b'x'; MAX_RECORD_CONTENT], 0));
        }
        assert_rejected(inbound, "inbound response exceeds").await;

        let mut body_limited = record(6, 1, b"Content-Type: text/plain\r\n\r\n", 0);
        body_limited.extend(record(6, 1, &vec![b'x'; MAX_RECORD_CONTENT], 0));
        body_limited.extend(record(6, 1, b"xx", 0));
        body_limited.extend(record(6, 1, b"", 0));
        body_limited.extend(end_request(0, 0));
        assert_rejected(body_limited, "CGI body exceeds").await;

        let oversized_headers = format!("X: {}\r\n\r\n", "x".repeat(MAX_CGI_HEADER_BYTES));
        let mut header_limited = record(6, 1, oversized_headers.as_bytes(), 0);
        header_limited.extend(record(6, 1, b"", 0));
        header_limited.extend(end_request(0, 0));
        assert_rejected(header_limited, "CGI headers exceed").await;

        let headers = (0..=MAX_CGI_HEADERS)
            .map(|index| format!("X-{index}: v"))
            .collect::<Vec<_>>()
            .join("\r\n")
            + "\r\n\r\n";
        let mut count_limited = record(6, 1, headers.as_bytes(), 0);
        count_limited.extend(record(6, 1, b"", 0));
        count_limited.extend(end_request(0, 0));
        assert_rejected(count_limited, "more than 32 headers").await;
    }

    #[tokio::test]
    async fn rejects_malformed_duplicate_or_non_success_cgi_headers() {
        for (cgi, expected) in [
            ("Missing-colon\r\n\r\nbody", "without colon"),
            ("Bad Name: value\r\n\r\nbody", "header name"),
            (
                "Content-Type: text/plain\r\ncontent-type: other\r\n\r\nbody",
                "duplicate CGI header",
            ),
            ("Status: nope\r\n\r\nbody", "malformed Status"),
            ("Status: 503 Nope\r\n\r\nbody", "not successful"),
        ] {
            let mut response = record(6, 1, cgi.as_bytes(), 0);
            response.extend(record(6, 1, b"", 0));
            response.extend(end_request(0, 0));
            assert_rejected(response, expected).await;
        }

        let mut missing_separator = record(6, 1, b"Content-Type: text/plain", 0);
        missing_separator.extend(record(6, 1, b"", 0));
        missing_separator.extend(end_request(0, 0));
        assert_rejected(missing_separator, "lacks CRLFCRLF").await;
    }

    #[tokio::test]
    async fn consumes_padding_and_handles_interleaving_split_delimiter_and_reassembly() {
        let mut response = record(6, 1, b"Content-Type: text/plain\r\nX-Test: yes\r", 5);
        response.extend(record(7, 1, b"", 3));
        response.extend(record(6, 1, b"\n\r", 7));
        response.extend(record(6, 1, b"\npart-", 1));
        response.extend(record(6, 1, b"one", 0));
        response.extend(record(6, 1, b"-two", 2));
        response.extend(record(6, 1, b"", 4));
        response.extend(end_request(0, 0));

        let result = scripted_request(response, &[]).await.unwrap();
        assert_eq!(result.body, b"part-one-two");
        assert_eq!(result.headers[1], ("X-Test".into(), "yes".into()));
    }

    #[tokio::test]
    async fn encodes_four_byte_param_lengths_and_chunks_stream_records() {
        let name = "N".repeat(128);
        let value = "V".repeat(129);
        let fixture = tempfile::tempdir_in("/private/tmp").unwrap();
        let socket = fixture.path().join("f.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            stream.write_all(&valid_response(b"ok")).await.unwrap();
            stream.shutdown().await.unwrap();
            request
        });
        request(
            &socket,
            &[(name.clone(), value.clone())],
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        let request = server.await.unwrap();

        assert_eq!(
            &request[..16],
            &[1, 1, 0, 1, 0, 8, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]
        );
        let params_header = &request[16..24];
        assert_eq!(params_header[1], FCGI_PARAMS);
        let params_len = u16::from_be_bytes([params_header[4], params_header[5]]) as usize;
        let params = &request[24..24 + params_len];
        assert_eq!(&params[..4], &(0x8000_0000_u32 | 128).to_be_bytes());
        assert_eq!(&params[4..8], &(0x8000_0000_u32 | 129).to_be_bytes());
        assert_eq!(&params[8..136], name.as_bytes());
        assert_eq!(&params[136..], value.as_bytes());

        let mut chunked = Vec::new();
        append_stream_records(
            &mut chunked,
            FCGI_PARAMS,
            REQUEST_ID,
            &vec![b'x'; MAX_RECORD_CONTENT + 1],
        );
        assert_eq!(u16::from_be_bytes([chunked[4], chunked[5]]), u16::MAX);
        let second = 8 + MAX_RECORD_CONTENT;
        assert_eq!(
            u16::from_be_bytes([chunked[second + 4], chunked[second + 5]]),
            1
        );
        assert_eq!(chunked.len(), MAX_RECORD_CONTENT + 1 + 16);

        let params_terminator = 24 + params_len;
        assert_eq!(
            &request[params_terminator..params_terminator + 8],
            &[1, 4, 0, 1, 0, 0, 0, 0]
        );
        assert_eq!(&request[params_terminator + 8..], &[1, 5, 0, 1, 0, 0, 0, 0]);
    }

    #[tokio::test]
    async fn outbound_param_limit_refuses_before_connect() {
        let fixture = tempfile::tempdir_in("/private/tmp").unwrap();
        let socket = fixture.path().join("f.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let error = request(
            &socket,
            &[("KEY".into(), "x".repeat(MAX_PARAMS_BYTES))],
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(error.contains("PARAMS exceed"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "oversized params must be rejected before socket I/O"
        );
    }

    #[tokio::test]
    async fn stalled_server_times_out_and_connection_is_cancelled() {
        let fixture = tempfile::tempdir_in("/private/tmp").unwrap();
        let socket = fixture.path().join("f.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            tokio::time::sleep(Duration::from_millis(75)).await;
            let mut byte = [0_u8; 1];
            stream.read(&mut byte).await.unwrap()
        });

        let error = request(&socket, &[], Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(error.contains("timeout during response header"));
        assert_eq!(server.await.unwrap(), 0, "timeout must drop the socket");
    }
}
