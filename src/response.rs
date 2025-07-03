use std::io;
use std::mem::MaybeUninit;

use crate::request::MAX_HEADERS;

use bytes::BytesMut;
pub struct Response<'a> {
    headers: [Header; MAX_HEADERS],
    headers_len: usize,
    status_message: StatusMessage,
    body: Body,
    rsp_buf: &'a mut BytesMut,
}

enum Body {
    Str(&'static str),
    Vec(Vec<u8>),
    Dummy,
}

enum Header {
    Str(&'static str),
    Vec(Vec<u8>),
    String(String),
}

struct StatusMessage {
    code: usize,
    msg: &'static str,
}

impl<'a> Response<'a> {
    pub(crate) fn new(rsp_buf: &'a mut BytesMut) -> Response<'a> {
        let mut headers: [MaybeUninit<Header>; MAX_HEADERS] =
            unsafe { MaybeUninit::uninit().assume_init() };

        for slot in &mut headers[..] {
            slot.write(Header::Str(""));
        }

        let headers = unsafe { std::mem::transmute::<_, [Header; MAX_HEADERS]>(headers) };

        Response {
            headers: headers,
            headers_len: 0,
            body: Body::Dummy,
            status_message: StatusMessage {
                code: 200,
                msg: "Ok",
            },
            rsp_buf,
        }
    }

    #[inline]
    pub fn status_code(&mut self, code: usize, msg: &'static str) -> &mut Self {
        self.status_message = StatusMessage { code, msg };
        self
    }

    #[inline]
    pub fn header(&mut self, header: &'static str) -> &mut Self {
        self.headers[self.headers_len] = Header::Str(header);
        self.headers_len += 1;
        self
    }

    #[inline]
    pub fn header_string(&mut self, header: String) -> &mut Self {
        self.headers[self.headers_len] = Header::String(header);
        self.headers_len += 1;
        self
    }

    #[inline]
    pub fn body(&mut self, s: &'static str) {
        self.body = Body::Str(s);
    }

    #[inline]
    pub fn body_vec(&mut self, v: Vec<u8>) {
        self.body = Body::Vec(v);
    }

    #[inline]
    pub fn body_mut(&mut self) -> &mut BytesMut {
        match self.body {
            Body::Dummy => {}
            Body::Str(s) => {
                self.rsp_buf.extend_from_slice(s.as_bytes());
                self.body = Body::Dummy;
            }
            Body::Vec(ref v) => {
                self.rsp_buf.extend_from_slice(v);
                self.body = Body::Dummy;
            }
        }
        self.rsp_buf
    }

    #[inline]
    fn body_len(&self) -> usize {
        match self.body {
            Body::Dummy => self.rsp_buf.len(),
            Body::Str(s) => s.len(),
            Body::Vec(ref v) => v.len(),
        }
    }

    #[inline]
    fn get_body(&mut self) -> &[u8] {
        match self.body {
            Body::Dummy => self.rsp_buf.as_ref(),
            Body::Str(s) => s.as_bytes(),
            Body::Vec(ref v) => v,
        }
    }
}

impl Drop for Response<'_> {
    fn drop(&mut self) {
        self.rsp_buf.clear();
    }
}

pub(crate) fn encode(mut rsp: Response, buf: &mut BytesMut) {
    if rsp.status_message.code == 200 {
        buf.extend_from_slice(b"HTTP/1.1 200 Ok\r\nServer: M\r\nDate: ");
    } else {
        buf.extend_from_slice(b"HTTP/1.1 ");
        let mut code = itoa::Buffer::new();
        buf.extend_from_slice(code.format(rsp.status_message.code).as_bytes());
        buf.extend_from_slice(b" ");
        buf.extend_from_slice(rsp.status_message.msg.as_bytes());
        //        buf.extend_from_slice(b"\r\nServer: M\r\nDate: ");
    }
    //  crate::date::append_date(buf);
    buf.extend_from_slice(b"\r\nContent-Length: ");
    let mut length = itoa::Buffer::new();
    buf.extend_from_slice(length.format(rsp.body_len()).as_bytes());

    let headers = unsafe { rsp.headers.get_unchecked(..rsp.headers_len) };
    for h in headers {
        buf.extend_from_slice(b"\r\n");
        match h {
            Header::Str(s) => buf.extend_from_slice(s.as_bytes()),
            Header::Vec(v) => buf.extend_from_slice(&v),
            Header::String(s) => buf.extend_from_slice(s.as_bytes()),
        }
    }

    buf.extend_from_slice(b"\r\n\r\n");
    buf.extend_from_slice(rsp.get_body());
}

#[cold]
pub(crate) fn encode_error(e: io::Error, buf: &mut BytesMut) {
    error!("error in service: err = {e:?}");
    let msg_string = e.to_string();
    let msg = msg_string.as_bytes();

    buf.extend_from_slice(b"HTTP/1.1 500 Internal Server Error\r\nServer: M\r\nDate: ");
    //crate::date::append_date(buf);
    buf.extend_from_slice(b"\r\nContent-Length: ");
    let mut length = itoa::Buffer::new();
    buf.extend_from_slice(length.format(msg.len()).as_bytes());

    buf.extend_from_slice(b"\r\n\r\n");
    buf.extend_from_slice(msg);
}
