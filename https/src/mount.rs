use std::error::Error;
use std::path::{Path, Component};
use std::fmt;

use sequence_trie::SequenceTrie;
use http::StatusCode;
use modifier::Set;

use request::{Url, Request};
use response::Response;
use handler::{HttpsError, HttpsResult, Handler};
use typemap;

/*
* 源url
*/
#[derive(Copy, Clone)]
pub struct OriginalUrl;

impl typemap::Key for OriginalUrl { type Value = Url; }

/*
* 匹配成功
*/
struct Match {
    handler: Box<Handler>,
    length: usize
}

/*
* 匹配失败
*/
#[derive(Debug)]
pub struct NoMatch;

impl Error for NoMatch {
    fn description(&self) -> &'static str { "No Match" }
}

impl fmt::Display for NoMatch {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.description())
    }
}

/**
* 挂载器
*/
pub struct Mount {
    inner: SequenceTrie<String, Match>
}

impl Handler for Mount {
    fn handle(&self, mut req: Request, res: Response) -> Option<(Request, Response, HttpsResult<()>)> {
        let matched = {
            let keys: Vec<_>;
            {
                let path = req.url.path();

                let key = match path.last() {
                    Some(s) if s.is_empty() => &path[..path.len() - 1],
                    _ => &path
                };

                keys = key.into_iter().map(|s| String::from(*s)).collect();
            }

            match self.inner.get_ancestor(&keys) {
                Some(matched) => matched,
                None => return Some((req, res.set(StatusCode::NOT_FOUND), Err(HttpsError::new(NoMatch)))),
            }
        };

        let is_outer_mount = !req.extensions.contains::<OriginalUrl>();
        if is_outer_mount {
            req.extensions.insert::<OriginalUrl>(req.url.clone());
        }

        let path = req.url.path()[matched.length..].join("/");
        req.url.as_mut().set_path(&path);

        match matched.handler.handle(req, res) {
            None => return None,
            Some((mut r, q, e)) => {
                r.url = match r.extensions.get::<OriginalUrl>() {
                    Some(original) => original.clone(),
                    None => panic!("OriginalUrl unexpectedly removed from req.extensions.")
                };
                if is_outer_mount {
                    r.extensions.remove::<OriginalUrl>();
                }
                return Some((r, q, e));
            }
        }
    }
}

impl Mount {
    /**
    * 构建一个挂载器
    * @returns 返回挂载器
    */
    pub fn new() -> Self {
        Mount {
            inner: SequenceTrie::new()
        }
    }

    /**
    * 在指定路由上挂载指定的处理器，路由必须为绝对路径
    * @param route 路由
    * @param handler 处理器
    * @returns 返回挂载器
    */
    pub fn mount<H: Handler>(&mut self, route: &str, handler: H) -> &mut Mount {
        let key: Vec<&str> = Path::new(route).components().flat_map(|c|
            match c {
                Component::RootDir => None,
                c => Some(c.as_os_str().to_str().unwrap())
            }
        ).collect();

        let match_length = key.len();
        self.inner.insert(key, Match {
            handler: Box::new(handler) as Box<Handler>,
            length: match_length,
        });
        self
    }
}
