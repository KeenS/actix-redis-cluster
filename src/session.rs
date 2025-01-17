use std::cell::RefCell;
use std::collections::HashMap;
use std::iter;
use std::rc::Rc;

use actix::prelude::*;
use actix_session::Session;
use actix_web::cookie::{Cookie, CookieJar, Key, SameSite};
use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::http::header::{self, HeaderValue};
use actix_web::{error, Error, HttpMessage};
use futures::future::{err, ok, Either, Future, FutureResult};
use futures::Poll;
use rand::{distributions::Alphanumeric, rngs::OsRng, Rng};
use time::Duration;

use crate::command::{self, Expiration, Get, Set};
use crate::redis::RedisActor;
use crate::RedisClusterActor;

/// Use redis as session storage.
///
/// You need to pass an address of the redis server and random value to the
/// constructor of `RedisSessionBackend`. This is private key for cookie
/// session, When this value is changed, all session data is lost.
///
/// Constructor panics if key length is less than 32 bytes.
#[derive(Clone)]
pub struct RedisSession(Rc<Inner>);

impl RedisSession {
    /// Create new redis session backend
    ///
    /// * `addr` - address of the redis server
    pub fn new<S: Into<String>>(addr: S, key: &[u8]) -> RedisSession {
        RedisSession(Rc::new(Inner {
            key: Key::from_master(key),
            ttl: "7200".to_owned(),
            addr: Redis::Redis(RedisActor::start(addr)),
            name: "actix-session".to_owned(),
            path: "/".to_owned(),
            domain: None,
            secure: false,
            max_age: Some(Duration::days(7)),
            same_site: None,
        }))
    }

    /// Create new redis session backend with redis cluster
    ///
    /// * `addrs` - addresses of the redis masters
    pub fn new_cluster<S: Into<String>>(addr: S, key: &[u8]) -> RedisSession {
        RedisSession(Rc::new(Inner {
            key: Key::from_master(key),
            ttl: "7200".to_owned(),
            addr: Redis::RedisCluster(RedisClusterActor::start(addr)),
            name: "actix-session".to_owned(),
            path: "/".to_owned(),
            domain: None,
            secure: false,
            max_age: Some(Duration::days(7)),
            same_site: None,
        }))
    }

    /// Set time to live in seconds for session value
    pub fn ttl(mut self, ttl: i64) -> Self {
        Rc::get_mut(&mut self.0).unwrap().ttl = format!("{}", ttl);
        self
    }

    /// Set custom cookie name for session id
    pub fn cookie_name(mut self, name: &str) -> Self {
        Rc::get_mut(&mut self.0).unwrap().name = name.to_owned();
        self
    }

    /// Set custom cookie path
    pub fn cookie_path(mut self, path: &str) -> Self {
        Rc::get_mut(&mut self.0).unwrap().path = path.to_owned();
        self
    }

    /// Set custom cookie domain
    pub fn cookie_domain(mut self, domain: &str) -> Self {
        Rc::get_mut(&mut self.0).unwrap().domain = Some(domain.to_owned());
        self
    }

    /// Set custom cookie secure
    /// If the `secure` field is set, a cookie will only be transmitted when the
    /// connection is secure - i.e. `https`
    pub fn cookie_secure(mut self, secure: bool) -> Self {
        Rc::get_mut(&mut self.0).unwrap().secure = secure;
        self
    }

    /// Set custom cookie max-age
    pub fn cookie_max_age(mut self, max_age: Duration) -> Self {
        Rc::get_mut(&mut self.0).unwrap().max_age = Some(max_age);
        self
    }

    /// Set custom cookie SameSite
    pub fn cookie_same_site(mut self, same_site: SameSite) -> Self {
        Rc::get_mut(&mut self.0).unwrap().same_site = Some(same_site);
        self
    }
}

impl<S, B> Transform<S> for RedisSession
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error>
        + 'static,
    S::Future: 'static,
    B: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = S::Error;
    type InitError = ();
    type Transform = RedisSessionMiddleware<S>;
    type Future = FutureResult<Self::Transform, Self::InitError>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(RedisSessionMiddleware {
            service: Rc::new(RefCell::new(service)),
            inner: self.0.clone(),
        })
    }
}

/// Cookie session middleware
#[derive(Clone)]
pub struct RedisSessionMiddleware<S: Service + 'static> {
    service: Rc<RefCell<S>>,
    inner: Rc<Inner>,
}

impl<S, B> Service for RedisSessionMiddleware<S>
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error>
        + 'static,
    S::Future: 'static,
    B: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = Box<dyn Future<Item = Self::Response, Error = Self::Error>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready()
    }

    fn call(&mut self, mut req: ServiceRequest) -> Self::Future {
        let mut srv = self.service.clone();
        let inner = self.inner.clone();

        Box::new(self.inner.load(&req).and_then(move |state| {
            let value = if let Some((state, value)) = state {
                Session::set_session(state.into_iter(), &mut req);
                Some(value)
            } else {
                None
            };

            srv.call(req).and_then(move |mut res| {
                if let (_status, Some(state)) = Session::get_changes(&mut res) {
                    Either::A(inner.update(res, state, value))
                } else {
                    Either::B(ok(res))
                }
            })
        }))
    }
}

struct Inner {
    key: Key,
    ttl: String,
    addr: Redis,
    name: String,
    path: String,
    domain: Option<String>,
    secure: bool,
    max_age: Option<Duration>,
    same_site: Option<SameSite>,
}

enum Redis {
    Redis(Addr<RedisActor>),
    RedisCluster(Addr<RedisClusterActor>),
}

impl Redis {
    fn send<M>(
        &self,
        msg: M,
    ) -> ResponseFuture<Result<M::Output, super::Error>, MailboxError>
    where
        M: command::Command
            + Message<Result = Result<<M as command::Command>::Output, super::Error>>
            + Send
            + 'static,
        <M as command::Command>::Output: Send + 'static,
    {
        use self::Redis::*;

        match self {
            Redis(addr) => Box::new(addr.send(msg)),
            RedisCluster(addr) => Box::new(addr.send(msg)),
        }
    }
}

impl Inner {
    fn load(
        &self,
        req: &ServiceRequest,
    ) -> impl Future<Item = Option<(HashMap<String, String>, String)>, Error = Error>
    {
        if let Ok(cookies) = req.cookies() {
            for cookie in cookies.iter() {
                if cookie.name() == self.name {
                    let mut jar = CookieJar::new();
                    jar.add_original(cookie.clone());
                    if let Some(cookie) = jar.signed(&self.key).get(&self.name) {
                        let value = cookie.value().to_owned();
                        return Either::A(
                            self.addr
                                .send(Get {
                                    key: cookie.value().into(),
                                })
                                .map_err(From::from)
                                .and_then(move |res| match res {
                                    Ok(Some(s)) => {
                                        if let Ok(val) = serde_json::from_slice(&s) {
                                            Ok(Some((val, value)))
                                        } else {
                                            Ok(None)
                                        }
                                    }
                                    Ok(None) => Ok(None),
                                    Err(err) => {
                                        Err(error::ErrorInternalServerError(err))
                                    }
                                }),
                        );
                    } else {
                        return Either::B(ok(None));
                    }
                }
            }
        }
        Either::B(ok(None))
    }

    fn update<B>(
        &self,
        mut res: ServiceResponse<B>,
        state: impl Iterator<Item = (String, String)>,
        value: Option<String>,
    ) -> impl Future<Item = ServiceResponse<B>, Error = Error> {
        let (value, jar) = if let Some(value) = value {
            (value.clone(), None)
        } else {
            let value: String = iter::repeat(())
                .map(|()| OsRng.sample(Alphanumeric))
                .take(32)
                .collect();

            // prepare session id cookie
            let mut cookie = Cookie::new(self.name.clone(), value.clone());
            cookie.set_path(self.path.clone());
            cookie.set_secure(self.secure);
            cookie.set_http_only(true);

            if let Some(ref domain) = self.domain {
                cookie.set_domain(domain.clone());
            }

            if let Some(max_age) = self.max_age {
                cookie.set_max_age(max_age);
            }

            if let Some(same_site) = self.same_site {
                cookie.set_same_site(same_site);
            }

            // set cookie
            let mut jar = CookieJar::new();
            jar.signed(&self.key).add(cookie);

            (value, Some(jar))
        };

        let state: HashMap<_, _> = state.collect();

        match serde_json::to_string(&state) {
            Err(e) => Either::A(err(e.into())),
            Ok(body) => Either::B(
                self.addr
                    .send(Set {
                        key: value,
                        value: body,
                        expiration: Expiration::Ex(self.ttl.clone()),
                    })
                    .map_err(Error::from)
                    .and_then(move |redis_result| match redis_result {
                        Ok(_) => {
                            if let Some(jar) = jar {
                                for cookie in jar.delta() {
                                    let val =
                                        HeaderValue::from_str(&cookie.to_string())?;
                                    res.headers_mut().append(header::SET_COOKIE, val);
                                }
                            }
                            Ok(res)
                        }
                        Err(err) => Err(error::ErrorInternalServerError(err)),
                    }),
            ),
        }
    }
}
