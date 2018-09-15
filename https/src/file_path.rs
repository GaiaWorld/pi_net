use std::iter::FromIterator;
use std::path::{Component, PathBuf, Path};
use std::fs::{self, Metadata};
use std::convert::AsRef;

use url_ext::percent_encoding::percent_decode;

use request::Request;

/*
* 文件访问路径
*/
pub struct RequestedPath {
    pub path: PathBuf,
}

impl RequestedPath {
    //构建一个文件访问路径
    pub fn new<P: AsRef<Path>>(root_path: P, request: &Request) -> Self {
        let decoded_req_path = PathBuf::from_iter(request.url.path().iter().map(decode_percents));
        let mut result = root_path.as_ref().to_path_buf();
        result.extend(&normalize_path(&decoded_req_path));
        RequestedPath { path: result }
    }

    //判断是否可以重定向
    pub fn should_redirect(&self, metadata: &Metadata, request: &Request) -> bool {
        let has_trailing_slash = match request.url.path().last() {
            Some(&"") => true,
            _ => false,
        };

        metadata.is_dir() && !has_trailing_slash
    }

    //获取指定文件路径
    pub fn get_file(self, metadata: &Metadata) -> Option<PathBuf> {
        if metadata.is_file() {
            //文件存在，则返回
            return Some(self.path);
        }

        let index_path = self.path.join("index.html");
        match fs::metadata(&index_path) {
            Ok(m) =>
                if m.is_file() {
                    //默认文件存在，则返回
                    Some(index_path)
                } else {
                    None
                },
            Err(_) => None,
        }
    }
}

//对path进行%解码
#[inline]
fn decode_percents(string: &&str) -> String {
    percent_decode(string.as_bytes()).decode_utf8().unwrap().into_owned()
}

//标准化路径
fn normalize_path(path: &Path) -> PathBuf {
    path.components().fold(PathBuf::new(), |mut result, p| {
        match p {
            Component::Normal(x) => {
                result.push(x);
                result
            }
            Component::ParentDir => {
                result.pop();
                result
            },
            _ => result
        }
    })
}

