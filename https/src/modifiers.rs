use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use modifier::Modifier;

use mime::{self, Mime};

use http::StatusCode;
use headers;
use modifier::Set;

use mime_guess::guess_mime_type_opt;
use request::{Url, Request};
use response::{BodyReader, WriteBody, Response};

impl Modifier<Response> for Mime {
    #[inline]
    fn modify(self, res: &mut Response) {
        res.headers
            .insert(headers::CONTENT_TYPE, self.as_ref().parse().unwrap());
    }
}

impl Modifier<Response> for Box<WriteBody> {
    #[inline]
    fn modify(self, res: &mut Response) {
        res.body = Some(self);
    }
}

impl<R: io::Read + Send + 'static> Modifier<Response> for BodyReader<R> {
    #[inline]
    fn modify(self, res: &mut Response) {
        res.body = Some(Box::new(self));
    }
}

impl Modifier<Response> for String {
    #[inline]
    fn modify(self, res: &mut Response) {
        self.into_bytes().modify(res);
    }
}

impl Modifier<Response> for Vec<u8> {
    #[inline]
    fn modify(self, res: &mut Response) {
        res.headers
            .insert(headers::CONTENT_LENGTH, (self.len() as u64).into());
        res.body = Some(Box::new(self));
    }
}

impl<'a> Modifier<Response> for &'a str {
    #[inline]
    fn modify(self, res: &mut Response) {
        self.to_owned().modify(res);
    }
}

impl<'a> Modifier<Response> for &'a [u8] {
    #[inline]
    fn modify(self, res: &mut Response) {
        self.to_vec().modify(res);
    }
}

impl Modifier<Response> for File {
    fn modify(self, res: &mut Response) {
        if let Ok(metadata) = self.metadata() {
            res.headers
                .insert(headers::CONTENT_LENGTH, metadata.len().into());
        }

        res.body = Some(Box::new(self));
    }
}

impl<'a> Modifier<Response> for &'a Path {
    fn modify(self, res: &mut Response) {
        File::open(self)
            .unwrap_or_else(|_| panic!("No such file: {}", self.display()))
            .modify(res);

        let mime = mime_for_path(self);
        res.set_mut(mime);
    }
}

impl Modifier<Response> for PathBuf {
    #[inline]
    fn modify(self, res: &mut Response) {
        self.as_path().modify(res);
    }
}

impl Modifier<Response> for StatusCode {
    fn modify(self, res: &mut Response) {
        res.status = Some(self);
    }
}

#[derive(Clone)]
pub struct Header<H>(pub H, pub headers::HeaderValue);

impl<H> Modifier<Response> for Header<H>
where
    H: headers::IntoHeaderName,
{
    fn modify(self, res: &mut Response) {
        res.headers.insert(self.0, self.1);
    }
}

impl<H> Modifier<Request> for Header<H>
where
    H: headers::IntoHeaderName,
{
    fn modify(self, res: &mut Request) {
        res.headers.insert(self.0, self.1);
    }
}

pub struct Redirect(pub Url);

impl Modifier<Response> for Redirect {
    fn modify(self, res: &mut Response) {
        let Redirect(url) = self;
        res.headers
            .insert(headers::LOCATION, url.to_string().parse().unwrap());
    }
}

pub struct RedirectRaw(pub String);

impl Modifier<Response> for RedirectRaw {
    fn modify(self, res: &mut Response) {
        let RedirectRaw(path) = self;
        res.headers.insert(headers::LOCATION, path.parse().unwrap());
    }
}

pub fn mime_for_path(path: &Path) -> Mime {
    guess_mime_type_opt(path).unwrap_or(mime::TEXT_PLAIN)
}
