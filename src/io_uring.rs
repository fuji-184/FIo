pub struct Res<'res> {
    pub status_code: u16,
    pub content_type: &'res str,
    pub headers: &'res [(&'res str, &'res str)],
    pub body: &'res str,
}

impl<'res> Res<'res> {
    pub fn ok(body: &'res str) -> Self {
        Self {
            status_code: 200,
            content_type: "text/plain",
            headers: &[],
            body: body,
        }
    }

    pub fn not_found() -> Self {
        Self {
            status_code: 404,
            content_type: "text/plain",
            headers: &[],
            body: "Not Found",
        }
    }
}
