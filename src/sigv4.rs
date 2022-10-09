use {
    crate::RequestId,
    async_trait::async_trait,
    chrono::Utc,
    derive_builder::Builder,
    http::method::Method,
    hyper::{body::Body, Request, Response},
    log::{trace, info},
    scratchstack_aws_signature::{
        canonical::get_content_type_and_charset, sigv4_validate_request, GetSigningKeyRequest, GetSigningKeyResponse,
        SignatureError, SignatureOptions, SignedHeaderRequirements,
    },
    scratchstack_errors::ServiceError,
    serde::Serialize,
    std::{
        any::type_name,
        error::Error,
        fmt::{Debug, Formatter, Result as FmtResult},
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    },
    tower::{BoxError, Service, ServiceExt},
};

/// AWSSigV4VerifierService implements a Hyper service that authenticates a request against AWS SigV4 signing protocol.
#[derive(Builder, Clone)]
pub struct AwsSigV4VerifierService<G, S, E>
where
    G: Service<GetSigningKeyRequest, Response = GetSigningKeyResponse, Error = BoxError> + Clone + Send + 'static,
    G::Future: Send,
    S: Service<Request<Body>, Response = Response<Body>, Error = BoxError> + Clone + Send + 'static,
    S::Future: Send,
    E: ErrorMapper,
{
    #[builder(setter(into))]
    region: String,

    #[builder(setter(into))]
    service: String,

    #[builder(default)]
    allowed_request_methods: Vec<Method>,

    #[builder(default)]
    allowed_content_types: Vec<String>,

    #[builder(default)]
    signed_header_requirements: SignedHeaderRequirements,

    get_signing_key: G,
    implementation: S,
    error_mapper: E,

    #[builder(default)]
    signature_options: SignatureOptions,
}

impl<G, S, E> AwsSigV4VerifierService<G, S, E>
where
    G: Service<GetSigningKeyRequest, Response = GetSigningKeyResponse, Error = BoxError> + Clone + Send + 'static,
    G::Future: Send,
    S: Service<Request<Body>, Response = Response<Body>, Error = BoxError> + Clone + Send + 'static,
    S::Future: Send,
    E: ErrorMapper,
{
    pub fn builder() -> AwsSigV4VerifierServiceBuilder<G, S, E> {
        AwsSigV4VerifierServiceBuilder::default()
    }

    #[inline]
    pub fn region(&self) -> &str {
        &self.region
    }

    #[inline]
    pub fn service(&self) -> &str {
        &self.service
    }

    #[inline]
    pub fn allowed_request_methods(&self) -> &Vec<Method> {
        &self.allowed_request_methods
    }

    #[inline]
    pub fn allowed_content_types(&self) -> &Vec<String> {
        &self.allowed_content_types
    }

    #[inline]
    pub fn signed_header_requirements(&self) -> &SignedHeaderRequirements {
        &self.signed_header_requirements
    }

    #[inline]
    pub fn get_signing_key(&self) -> &G {
        &self.get_signing_key
    }

    #[inline]
    pub fn implementation(&self) -> &S {
        &self.implementation
    }

    #[inline]
    pub fn error_mapper(&self) -> &E {
        &self.error_mapper
    }

    #[inline]
    pub fn signature_options(&self) -> &SignatureOptions {
        &self.signature_options
    }
}

impl<G, S, E> Debug for AwsSigV4VerifierService<G, S, E>
where
    G: Service<GetSigningKeyRequest, Response = GetSigningKeyResponse, Error = BoxError> + Clone + Send + 'static,
    G::Future: Send,
    S: Service<Request<Body>, Response = Response<Body>, Error = BoxError> + Clone + Send + 'static,
    S::Future: Send,
    E: ErrorMapper,
{
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        f.debug_struct("AwsSigV4VerifierService")
            .field("region", &self.region)
            .field("service", &self.service)
            .field("get_signing_key", &type_name::<G>())
            .field("implementation", &type_name::<S>())
            .field("error_handler", &type_name::<E>())
            .field("signature_options", &self.signature_options)
            .finish()
    }
}

impl<G, S, E> Service<Request<Body>> for AwsSigV4VerifierService<G, S, E>
where
    G: Service<GetSigningKeyRequest, Response = GetSigningKeyResponse, Error = BoxError> + Clone + Send + 'static,
    G::Future: Send,
    S: Service<Request<Body>, Response = Response<Body>, Error = BoxError> + Clone + Send + 'static,
    S::Future: Send,
    E: ErrorMapper,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, c: &mut Context) -> Poll<Result<(), Self::Error>> {
        match self.get_signing_key.poll_ready(c) {
            Poll::Ready(r) => match r {
                Ok(()) => match self.implementation.poll_ready(c) {
                    Poll::Ready(r) => match r {
                        Ok(()) => Poll::Ready(Ok(())),
                        Err(e) => Poll::Ready(Err(e)),
                    },
                    Poll::Pending => Poll::Pending,
                },
                Err(e) => Poll::Ready(Err(e)),
            },
            Poll::Pending => Poll::Pending,
        }
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        let region = self.region.clone();
        let service = self.service.clone();
        let allowed_request_methods = self.allowed_request_methods.clone();
        let allowed_content_types = self.allowed_content_types.clone();
        let signed_header_requirements = self.signed_header_requirements.clone();
        let mut get_signing_key = self.get_signing_key.clone();
        let implementation = self.implementation.clone();
        let error_mapper = self.error_mapper.clone();
        let signature_options = self.signature_options;

        Box::pin(async move {
            // Do we have a request id?
            let extensions = req.extensions_mut();
            let request_id = match extensions.get::<RequestId>() {
                Some(request_id) => *request_id,
                None => {
                    let new_request_id = RequestId::new();
                    trace!("Generated request-id: {}", new_request_id);
                    extensions.insert(new_request_id);

                    new_request_id
                }
            };

            // Rule 2: Is the request method appropriate?
            if !allowed_request_methods.is_empty() && !allowed_request_methods.contains(req.method()) {
                return error_mapper
                    .map_error(
                        SignatureError::InvalidRequestMethod(format!("Unsupported request method '{}", req.method()))
                            .into(),
                        Some(request_id),
                    )
                    .await;
            }

            // Rule 3: Is the content type appropriate?
            if let Some(ctc) = get_content_type_and_charset(req.headers()) {
                trace!("Content-Type: {}", ctc.content_type);
                if !allowed_content_types.contains(&ctc.content_type) {
                    // Rusoto and some other clients set Content-Type to application/octet-stream for GET requests <sigh>
                    let mut get_ok = false;

                    if req.method() == Method::GET {
                        get_ok = req.headers().get("content-length").is_none();
                        get_ok |= req.headers().get("expect").is_none();
                        if let Some(te) = req.headers().get("transfer-encoding") {
                            let te = String::from_utf8_lossy(te.as_bytes());
                            for part in te.split(',') {
                                if part.trim() == "chunked" {
                                    get_ok = false;
                                    break;
                                }
                            }
                        }
                    }

                    if !get_ok {
                        info!("Invalid Content-Type: {}", ctc.content_type);
                        return error_mapper
                            .map_error(
                                SignatureError::InvalidContentType(
                                    "The content-type of the request is unsupported".to_string(),
                                )
                                .into(),
                                Some(request_id),
                            )
                            .await;
                    }
                }
            }

            let result = sigv4_validate_request(
                req,
                region.as_str(),
                service.as_str(),
                &mut get_signing_key,
                Utc::now(),
                &signed_header_requirements,
                signature_options,
            )
            .await;

            match result {
                Ok((mut parts, body, principal, session_data)) => {
                    let body = Body::from(body);
                    parts.extensions.insert(principal);
                    parts.extensions.insert(session_data);
                    let req = Request::from_parts(parts, body);
                    implementation.oneshot(req).await.map_err(Into::into)
                }
                Err(e) => error_mapper.map_error(e, Some(request_id)).await,
            }
        })
    }
}

#[async_trait]
pub trait ErrorMapper: Clone + Send + 'static {
    async fn map_error(self, error: BoxError, request_id: Option<RequestId>) -> Result<Response<Body>, BoxError>;
}

#[derive(Clone)]
pub struct XmlErrorMapper {
    namespace: String,
}

impl XmlErrorMapper {
    pub fn new(namespace: &str) -> Self {
        XmlErrorMapper {
            namespace: namespace.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename = "ErrorResponse")]
pub struct XmlErrorResponse {
    pub xmlns: String,

    #[serde(rename = "Error")]
    pub error: XmlError,

    #[serde(rename = "$unflatten=RequestId", skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
}

#[derive(Debug, Clone, Serialize)]
pub struct XmlError {
    #[serde(rename = "$unflatten=Type")]
    pub r#type: String,

    #[serde(rename = "$unflatten=Code")]
    pub code: String,

    #[serde(rename = "$unflatten=Message", skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl From<&SignatureError> for XmlError {
    fn from(error: &SignatureError) -> Self {
        XmlError {
            r#type: if error.http_status().as_u16() >= 500 {
                "Receiver"
            } else {
                "Sender"
            }
            .to_string(),
            code: error.error_code().to_string(),
            message: {
                let message = error.to_string();
                if message.is_empty() {
                    None
                } else {
                    Some(message)
                }
            },
        }
    }
}

#[async_trait]
impl ErrorMapper for XmlErrorMapper {
    async fn map_error(self, e: BoxError, request_id: Option<RequestId>) -> Result<Response<Body>, BoxError> {
        match e.downcast::<SignatureError>() {
            Ok(e) => {
                let xml_response = XmlErrorResponse {
                    xmlns: self.namespace,
                    error: XmlError::from(e.as_ref()),
                    request_id,
                };

                let body = Body::from(quick_xml::se::to_string(&xml_response).unwrap());
                let result: Result<Response<Body>, Box<dyn Error + Send + Sync>> = Response::builder()
                    .status(e.http_status())
                    .header("Content-Type", "text/xml; charset=utf-8")
                    .body(body)
                    .map_err(Into::into);
                result
            }
            Err(any) => Err(any),
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{AwsSigV4VerifierService, XmlErrorMapper},
        futures::stream::StreamExt,
        http::StatusCode,
        hyper::{
            client::{connect::dns::GaiResolver, HttpConnector},
            server::conn::AddrStream,
            service::{make_service_fn, service_fn},
            Body, Request, Response, Server,
        },
        log::info,
        pretty_assertions::assert_eq,
        regex::Regex,
        rusoto_core::{DispatchSignedRequest, HttpClient, Region},
        rusoto_credential::AwsCredentials,
        rusoto_signature::SignedRequest,
        scratchstack_aws_principal::{Principal, SessionData, User},
        scratchstack_aws_signature::{
            service_for_signing_key_fn, GetSigningKeyRequest, GetSigningKeyResponse, KSecretKey, SignatureError,
        },
        std::{
            convert::Infallible,
            future::Future,
            net::{Ipv6Addr, SocketAddr, SocketAddrV6},
            pin::Pin,
            task::{Context, Poll},
            time::Duration,
        },
        tower::{BoxError, Service},
    };

    const TEST_ACCESS_KEY: &str = "AKIDEXAMPLE";
    const TEST_SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

    #[test_log::test(tokio::test)]
    async fn test_fn_wrapper() {
        let sigfn = service_for_signing_key_fn(get_creds_fn);
        let wrapped = service_fn(hello_response);
        let make_svc = make_service_fn(|_socket: &AddrStream| async move {
            let err_handler = XmlErrorMapper::new("service_namespace");
            let verifier_svc = AwsSigV4VerifierService::builder()
                .region("local")
                .service("service")
                .get_signing_key(sigfn)
                .implementation(wrapped)
                .error_mapper(err_handler)
                .build()
                .unwrap();
            // Make sure we can debug print the verifier service.
            let _ = format!("{:?}", verifier_svc);
            Ok::<_, Infallible>(verifier_svc)
        });
        let server = Server::bind(&SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0))).serve(make_svc);
        let addr = server.local_addr();
        let port = match addr {
            SocketAddr::V6(sa) => sa.port(),
            SocketAddr::V4(sa) => sa.port(),
        };
        info!("Server listening on port {}", port);
        let mut connector = HttpConnector::new_with_resolver(GaiResolver::new());
        connector.set_connect_timeout(Some(Duration::from_millis(10)));
        let client = HttpClient::<HttpConnector<GaiResolver>>::from_connector(connector);
        match server
            .with_graceful_shutdown(async {
                let region = Region::Custom {
                    name: "local".to_owned(),
                    endpoint: format!("http://[::1]:{}", port),
                };
                let mut sr = SignedRequest::new("GET", "service", &region, "/");

                sr.sign(&AwsCredentials::new(TEST_ACCESS_KEY, TEST_SECRET_KEY, None, None));
                match client.dispatch(sr, Some(Duration::from_millis(100))).await {
                    Ok(r) => {
                        eprintln!("Response from server: {:?}", r.status);

                        let mut body = r.body;
                        while let Some(b_result) = body.next().await {
                            match b_result {
                                Ok(bytes) => eprint!("{:?}", bytes),
                                Err(e) => {
                                    eprintln!("Error while ready body: {:?}", e);
                                    break;
                                }
                            }
                        }
                        eprintln!();
                        assert_eq!(r.status, StatusCode::OK);
                    }
                    Err(e) => panic!("Error from server: {:?}", e),
                };
            })
            .await
        {
            Ok(()) => println!("Server shutdown normally"),
            Err(e) => panic!("Server shutdown with error {:?}", e),
        }
    }

    #[test_log::test(tokio::test)]
    async fn test_svc_wrapper() {
        let make_svc = SpawnDummyHelloService {};
        let server = Server::bind(&SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 5938, 0, 0))).serve(make_svc);
        let addr = server.local_addr();
        let port = match addr {
            SocketAddr::V6(sa) => sa.port(),
            SocketAddr::V4(sa) => sa.port(),
        };
        info!("Server listening on port {}", port);
        let mut connector = HttpConnector::new_with_resolver(GaiResolver::new());
        connector.set_connect_timeout(Some(Duration::from_millis(10)));
        let client = HttpClient::<HttpConnector<GaiResolver>>::from_connector(connector);
        let mut status = StatusCode::OK;
        match server
            .with_graceful_shutdown(async {
                let region = Region::Custom {
                    name: "local".to_owned(),
                    endpoint: format!("http://[::1]:{}", port),
                };
                let mut sr = SignedRequest::new("GET", "service", &region, "/");
                sr.sign(&AwsCredentials::new(TEST_ACCESS_KEY, TEST_SECRET_KEY, None, None));
                match client.dispatch(sr, Some(Duration::from_millis(100))).await {
                    Ok(r) => {
                        eprintln!("Response from server: {:?}", r.status);

                        let mut body = r.body;
                        while let Some(b_result) = body.next().await {
                            match b_result {
                                Ok(bytes) => eprint!("{:?}", bytes),
                                Err(e) => {
                                    eprintln!("Error while ready body: {:?}", e);
                                    break;
                                }
                            }
                        }
                        eprintln!();
                        status = r.status;
                    }
                    Err(e) => panic!("Error from server: {:?}", e),
                };
            })
            .await
        {
            Ok(()) => println!("Server shutdown normally"),
            Err(e) => panic!("Server shutdown with error {:?}", e),
        }

        assert_eq!(status, StatusCode::OK);
    }

    #[test_log::test(tokio::test)]
    async fn test_svc_wrapper_bad_creds() {
        let make_svc = SpawnDummyHelloService {};
        let server = Server::bind(&SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0))).serve(make_svc);
        let addr = server.local_addr();
        let port = match addr {
            SocketAddr::V6(sa) => sa.port(),
            SocketAddr::V4(sa) => sa.port(),
        };
        info!("Server listening on port {}", port);
        let mut connector = HttpConnector::new_with_resolver(GaiResolver::new());
        connector.set_connect_timeout(Some(Duration::from_millis(100)));
        let client = HttpClient::<HttpConnector<GaiResolver>>::from_connector(connector);
        match server
            .with_graceful_shutdown(async {
                let region = Region::Custom {
                    name: "local".to_owned(),
                    endpoint: format!("http://[::1]:{}", port),
                };
                let mut sr = SignedRequest::new("GET", "service", &region, "/");
                sr.sign(&AwsCredentials::new(TEST_ACCESS_KEY, "WRONGKEY", None, None));
                match client.dispatch(sr, Some(Duration::from_millis(100))).await {
                    Ok(r) => {
                        eprintln!("Response from server: {:?}", r.status);

                        let mut body = Vec::with_capacity(1024);
                        let mut body_stream = r.body;
                        while let Some(b_result) = body_stream.next().await {
                            match b_result {
                                Ok(bytes) => {
                                    eprint!("{:?}", bytes);
                                    body.extend_from_slice(&bytes);
                                },
                                Err(e) => {
                                    eprintln!("Error while ready body: {:?}", e);
                                    break;
                                }
                            }
                        }
                        eprintln!();
                        assert_eq!(r.status, 403);
                        let body_str = String::from_utf8(body).unwrap();
                        // Remove the RequestId from the body.
                        let body_str = Regex::new("<RequestId>[-0-9a-f]+</RequestId>").unwrap().replace_all(&body_str, "");
                        
                        assert_eq!(&body_str, r#"<ErrorResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/"><Error><Type>Sender</Type><Code>SignatureDoesNotMatch</Code><Message>The request signature we calculated does not match the signature you provided. Check your AWS Secret Access Key and signing method. Consult the service documentation for details.</Message></Error></ErrorResponse>"#);
                    }
                    Err(e) => panic!("Error from server: {:?}", e),
                };
            })
            .await
        {
            Ok(()) => println!("Server shutdown normally"),
            Err(e) => panic!("Server shutdown with error {:?}", e),
        }
    }

    #[test_log::test(tokio::test)]
    async fn test_svc_wrapper_backend_failure() {
        let make_svc = SpawnBadBackendService {};
        let server = Server::bind(&SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0))).serve(make_svc);
        let addr = server.local_addr();
        let port = match addr {
            SocketAddr::V6(sa) => sa.port(),
            SocketAddr::V4(sa) => sa.port(),
        };
        info!("Server listening on port {}", port);
        let mut connector = HttpConnector::new_with_resolver(GaiResolver::new());
        connector.set_connect_timeout(Some(Duration::from_millis(100)));
        let client = HttpClient::<HttpConnector<GaiResolver>>::from_connector(connector);
        match server
            .with_graceful_shutdown(async {
                let region = Region::Custom {
                    name: "local".to_owned(),
                    endpoint: format!("http://[::1]:{}", port),
                };
                let mut sr = SignedRequest::new("GET", "service", &region, "/");
                sr.sign(&AwsCredentials::new(TEST_ACCESS_KEY, TEST_SECRET_KEY, None, None));
                match client.dispatch(sr, Some(Duration::from_millis(100))).await {
                    Ok(r) => panic!("Expected an error, got {}", r.status),
                    Err(e) => eprintln!("Got expected server error: {:?}", e),
                };
            })
            .await
        {
            Ok(()) => println!("Server shutdown normally"),
            Err(e) => panic!("Server shutdown with error {:?}", e),
        }
    }

    async fn get_creds_fn(request: GetSigningKeyRequest) -> Result<GetSigningKeyResponse, BoxError> {
        if request.access_key == TEST_ACCESS_KEY {
            let k_secret = KSecretKey::from_str(TEST_SECRET_KEY);
            let k_signing =
                k_secret.to_ksigning(request.request_date, request.region.as_str(), request.service.as_str());
            let principal = Principal::from(vec![User::new("aws", "123456789012", "/", "test").unwrap().into()]);
            Ok(GetSigningKeyResponse {
                principal,
                session_data: SessionData::default(),
                signing_key: k_signing,
            })
        } else {
            Err(Box::new(SignatureError::InvalidClientTokenId(
                "The AWS access key provided does not exist in our records".to_string(),
            )))
        }
    }

    async fn hello_response(_req: Request<Body>) -> Result<Response<Body>, BoxError> {
        Ok(Response::new(Body::from("Hello world")))
    }

    #[derive(Clone)]
    struct SpawnDummyHelloService {}
    impl Service<&AddrStream> for SpawnDummyHelloService {
        type Response = AwsSigV4VerifierService<GetDummyCreds, HelloService, XmlErrorMapper>;
        type Error = BoxError;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _c: &mut Context) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _addr: &AddrStream) -> Self::Future {
            Box::pin(async move {
                Ok(AwsSigV4VerifierService::builder()
                    .region("local")
                    .service("service")
                    .get_signing_key(GetDummyCreds {})
                    .implementation(HelloService {})
                    .error_mapper(XmlErrorMapper::new("https://sts.amazonaws.com/doc/2011-06-15/"))
                    .build()
                    .unwrap())
            })
        }
    }

    #[derive(Clone)]
    struct GetDummyCreds {}

    impl GetDummyCreds {
        async fn get_signing_key(req: GetSigningKeyRequest) -> Result<GetSigningKeyResponse, BoxError> {
            if let Some(ref token) = req.session_token {
                match token.as_str() {
                    "invalid" => {
                        return Err(Box::new(SignatureError::InvalidClientTokenId(
                            "The security token included in the request is invalid".to_string(),
                        )))
                    }
                    "expired" => {
                        return Err(Box::new(SignatureError::ExpiredToken(
                            "The security token included in the request is expired".to_string(),
                        )))
                    }
                    _ => (),
                }
            }

            if req.access_key == TEST_ACCESS_KEY {
                let k_secret = KSecretKey::from_str(TEST_SECRET_KEY);
                let signing_key = k_secret.to_ksigning(req.request_date, req.region.as_str(), req.service.as_str());
                let principal = Principal::from(vec![User::new("aws", "123456789012", "/", "test").unwrap().into()]);
                Ok(GetSigningKeyResponse {
                    principal,
                    session_data: SessionData::default(),
                    signing_key,
                })
            } else {
                Err(SignatureError::InvalidClientTokenId(
                    "The AWS access key provided does not exist in our records".to_string(),
                )
                .into())
            }
        }
    }

    impl Service<GetSigningKeyRequest> for GetDummyCreds {
        type Response = GetSigningKeyResponse;
        type Error = BoxError;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _c: &mut Context) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: GetSigningKeyRequest) -> Self::Future {
            Box::pin(async move { GetDummyCreds::get_signing_key(req).await })
        }
    }

    #[derive(Clone)]
    struct HelloService {}
    impl Service<Request<Body>> for HelloService {
        type Response = Response<Body>;
        type Error = BoxError;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _c: &mut Context) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: Request<Body>) -> Self::Future {
            Box::pin(async move {
                let (parts, _body) = req.into_parts();
                let principal = parts.extensions.get::<Principal>();

                let (status, body) = match principal {
                    Some(principal) => (StatusCode::OK, format!("Hello {:?}", principal)),
                    None => (StatusCode::UNAUTHORIZED, "Unauthorized".to_string()),
                };

                match Response::builder().status(status).header("Content-Type", "text/plain").body(Body::from(body)) {
                    Ok(r) => Ok(r),
                    Err(e) => {
                        eprintln!("Response builder: error: {:?}", e);
                        Err(e.into())
                    }
                }
            })
        }
    }

    #[derive(Clone)]
    struct SpawnBadBackendService {}
    impl Service<&AddrStream> for SpawnBadBackendService {
        type Response = AwsSigV4VerifierService<BadGetCredsService, HelloService, XmlErrorMapper>;
        type Error = BoxError;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _c: &mut Context) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _addr: &AddrStream) -> Self::Future {
            Box::pin(async move {
                Ok(AwsSigV4VerifierService::builder()
                    .region("local")
                    .service("service")
                    .get_signing_key(BadGetCredsService {
                        calls: 0,
                    })
                    .implementation(HelloService {})
                    .error_mapper(XmlErrorMapper::new("service-ns"))
                    .build()
                    .unwrap())
            })
        }
    }

    #[derive(Clone)]
    struct BadGetCredsService {
        calls: usize,
    }

    impl Service<GetSigningKeyRequest> for BadGetCredsService {
        type Response = GetSigningKeyResponse;
        type Error = BoxError;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
            self.calls += 1;
            match self.calls {
                0..=1 => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                _ => Poll::Ready(Err(Box::new(String::from_utf8(b"\x80".to_vec()).unwrap_err()))),
            }
        }

        fn call(&mut self, _req: GetSigningKeyRequest) -> Self::Future {
            Box::pin(async move { Err(SignatureError::InternalServiceError("Internal Failure".into()).into()) })
        }
    }
}
