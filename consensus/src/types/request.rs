use crypto::hash::Hash;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Request<T> {
    req_hash: Hash<T>,
}

impl<T> Request<T> {
    pub fn new(req_hash: Hash<T>) -> Self {
        Self { req_hash }
    }

    pub fn request_hash(&self) -> &Hash<T> {
        &self.req_hash
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Response<T> {
    req_hash: Hash<T>,
    response: T,
}

impl<T> Response<T> {
    pub fn new(
        req_hash: Hash<T>,
        response: T,
    ) -> Self {
        Self { req_hash, response }
    }

    pub fn request_hash(&self) -> &Hash<T> {
        &self.req_hash
    }

    pub fn response(self) -> T {
        self.response
    }
}
