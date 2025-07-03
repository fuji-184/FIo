use crate::http_server::reserve_buf;
use bytes::{Buf, BufMut, BytesMut};
use std::collections::HashMap;
use std::fmt;
use std::mem::{self, MaybeUninit};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{io, slice};
use tokio::io::AsyncReadExt;
use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};
use tokio::net::TcpStream;

pub(crate) const MAX_HEADERS: usize = 16;

pub struct BodyReader<'buf, 'stream> {
    req_buf: &'buf mut BytesMut,
    body_limit: usize,
    total_read: usize,
    stream: &'stream mut TcpStream,
}

impl<'buf, 'stream> AsyncRead for BodyReader<'buf, 'stream> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let remaining = self.body_limit - self.total_read;
        if remaining == 0 {
            return Poll::Ready(Ok(()));
        }

        if !self.req_buf.is_empty() {
            let to_copy = remaining.min(buf.remaining()).min(self.req_buf.len());
            buf.put_slice(&self.req_buf.split_to(to_copy));
            self.total_read += to_copy;
            return Poll::Ready(Ok(()));
        }

        reserve_buf(self.req_buf);

        let chunk_mut = self.req_buf.chunk_mut();
        let read_len = chunk_mut.len().min(remaining);
        let dst = unsafe {
            slice::from_raw_parts_mut(chunk_mut.as_mut_ptr() as *mut MaybeUninit<u8>, read_len)
        };

        let mut read_buf = ReadBuf::uninit(dst);
        let stream = Pin::new(&mut *self.stream);

        match stream.poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => {
                let bytes_read = read_buf.filled().len();
                unsafe {
                    self.req_buf.advance_mut(bytes_read);
                }

                let to_copy = remaining.min(buf.remaining()).min(bytes_read);
                if to_copy > 0 {
                    buf.put_slice(&self.req_buf.split_to(to_copy));
                    self.total_read += to_copy;
                }

                Poll::Ready(Ok(()))
            }
            poll => poll,
        }
    }
}

impl<'buf, 'stream> AsyncBufRead for BodyReader<'buf, 'stream> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = unsafe { self.get_unchecked_mut() };
        let remaining = this.body_limit - this.total_read;

        if remaining == 0
            || this.req_buf.is_empty() && {
                reserve_buf(this.req_buf);

                let chunk_mut = this.req_buf.chunk_mut();
                let read_len = chunk_mut.len().min(remaining);
                let dst = unsafe {
                    slice::from_raw_parts_mut(
                        chunk_mut.as_mut_ptr() as *mut MaybeUninit<u8>,
                        read_len,
                    )
                };

                let mut read_buf = ReadBuf::uninit(dst);
                let stream = Pin::new(&mut *this.stream);

                match stream.poll_read(cx, &mut read_buf) {
                    Poll::Ready(Ok(())) => {
                        let bytes_read = read_buf.filled().len();
                        if bytes_read == 0 {
                            return Poll::Ready(Ok(&[]));
                        }
                        unsafe {
                            this.req_buf.advance_mut(bytes_read);
                        }
                        false
                    }
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                }
            }
        {
            return Poll::Ready(Ok(&[]));
        }

        let available = this.req_buf.len().min(remaining);
        Poll::Ready(Ok(&this.req_buf.chunk()[..available]))
    }

    #[inline(always)]
    fn consume(mut self: Pin<&mut Self>, amt: usize) {
        self.total_read += amt;
        self.req_buf.advance(amt);
    }
}

impl<'buf, 'stream> Drop for BodyReader<'buf, 'stream> {
    #[inline]
    fn drop(&mut self) {
        let remaining_to_consume = (self.body_limit - self.total_read).min(self.req_buf.len());
        self.total_read += remaining_to_consume;
        self.req_buf.advance(remaining_to_consume);
    }
}

pub struct Request<'buf, 'header, 'stream> {
    req: httparse::Request<'header, 'buf>,
    req_buf: &'buf mut BytesMut,
    stream: &'stream mut TcpStream,
    post_params: Option<HashMap<String, String>>,
}

impl<'buf, 'header, 'stream> Request<'buf, 'header, 'stream> {
    #[inline(always)]
    pub fn method(&self) -> &str {
        unsafe { self.req.method.unwrap_unchecked() }
    }

    #[inline(always)]
    pub fn path(&self) -> &str {
        unsafe { self.req.path.unwrap_unchecked() }
    }

    #[inline(always)]
    pub fn version(&self) -> u8 {
        unsafe { self.req.version.unwrap_unchecked() }
    }

    #[inline(always)]
    pub fn headers(&self) -> &[httparse::Header<'_>] {
        self.req.headers
    }

    #[inline]
    pub fn body(self) -> BodyReader<'buf, 'stream> {
        BodyReader {
            body_limit: self.content_length(),
            total_read: 0,
            stream: self.stream,
            req_buf: self.req_buf,
        }
    }

    #[inline]
    pub fn url_params(&self) -> HashMap<String, String> {
        let mut params = HashMap::new();

        unsafe {
            let full_path = self.req.path.unwrap_unchecked();
            let query = match full_path.find('?') {
                Some(pos) => &full_path[pos + 1..],
                None => "",
            };

            params = url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect::<HashMap<String, String>>();
        }

        params
    }

    #[inline]
    pub async fn post_params(&mut self) -> Option<&HashMap<String, String>> {
        if self.post_params.is_none() {
            let mut body = String::new();
            let mut body_reader = BodyReader {
                body_limit: self.content_length(),
                total_read: 0,
                stream: self.stream,
                req_buf: self.req_buf,
            };
            body_reader.read_to_string(&mut body).await.ok()?;

            let parsed = parse_form_urlencoded(&body);
            self.post_params = Some(parsed);
        }

        self.post_params.as_ref()
    }

    fn content_length(&self) -> usize {
        if self.req.headers.is_empty() {
            return 0;
        }

        for header in self.req.headers.iter() {
            if header.name.len() == 14 {
                let name_bytes = header.name.as_bytes();
                if name_bytes[0..8].eq_ignore_ascii_case(b"content-")
                    && name_bytes[8..14].eq_ignore_ascii_case(b"length")
                {
                    if !header.value.is_empty() {
                        return parse_content_length_fast(header.value);
                    }
                }
            }
        }
        0
    }
}

#[inline(always)]
fn parse_content_length_fast(bytes: &[u8]) -> usize {
    let mut result = 0usize;
    let mut i = 0;

    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }

    while i < bytes.len() {
        let byte = bytes[i];
        if byte >= b'0' && byte <= b'9' {
            result = result * 10 + (byte - b'0') as usize;
            i += 1;
        } else {
            break;
        }
    }

    result
}

#[inline]
fn parse_form_urlencoded(body: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();

    for pair in body.split('&') {
        let mut parts = pair.splitn(2, '=');
        if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
            let key = percent_decode(k);
            let val = percent_decode(v);
            params.insert(key, val);
        }
    }

    params
}

#[inline]
fn percent_decode(s: &str) -> String {
    let mut result = Vec::with_capacity(s.len());
    let mut chars = s.as_bytes().iter().cloned();

    while let Some(c) = chars.next() {
        match c {
            b'%' => {
                let hi = chars.next().unwrap_or(b'0');
                let lo = chars.next().unwrap_or(b'0');
                let hex = [hi, lo];
                let byte = u8::from_str_radix(std::str::from_utf8(&hex).unwrap_or("00"), 16)
                    .unwrap_or(b'?');
                result.push(byte);
            }
            b'+' => result.push(b' '),
            b => result.push(b),
        }
    }

    String::from_utf8(result).unwrap_or_default()
}

impl fmt::Debug for Request<'_, '_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<HTTP Request {} {}>", self.method(), self.path())
    }
}

#[inline(always)]
pub fn decode<'header, 'buf, 'stream>(
    headers: &'header mut [MaybeUninit<httparse::Header<'buf>>; MAX_HEADERS],
    req_buf: &'buf mut BytesMut,
    stream: &'stream mut TcpStream,
) -> io::Result<Option<Request<'buf, 'header, 'stream>>> {
    let mut req = httparse::Request::new(&mut []);

    let buf: &[u8] = unsafe { mem::transmute(req_buf.chunk()) };

    let status = req
        .parse_with_uninit_headers(buf, headers)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let consumed_bytes = match status {
        httparse::Status::Complete(len) => len,
        httparse::Status::Partial => return Ok(None),
    };

    req_buf.advance(consumed_bytes);

    Ok(Some(Request {
        req,
        req_buf,
        stream,
        post_params: None,
    }))
}
