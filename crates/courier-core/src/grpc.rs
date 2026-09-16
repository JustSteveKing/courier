//! gRPC without generated code: descriptors come from `.proto` files or from the server's
//! own reflection service, messages are written as JSON, and calls go out with a codec that
//! converts between the two at send time.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use prost::Message as _;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor, MethodDescriptor, SerializeOptions};

/// A method as the interface needs to know it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Method {
    /// Fully qualified, e.g. `pets.PetService`.
    pub service: String,
    pub name: String,
    pub client_streaming: bool,
    pub server_streaming: bool,
}

impl Method {
    /// The path a gRPC call is made on.
    pub fn path(&self) -> String {
        format!("/{}/{}", self.service, self.name)
    }

    /// `PetService / GetPet`, for the picker.
    pub fn label(&self) -> String {
        let service = self.service.rsplit('.').next().unwrap_or(&self.service);
        format!("{service} / {}", self.name)
    }

    /// Whether either side streams, so the interface knows to show a timeline.
    pub fn streaming(&self) -> bool {
        self.client_streaming || self.server_streaming
    }
}

/// Everything known about a server's API, and where it came from.
#[derive(Clone, Debug)]
pub struct Schema {
    pub pool: DescriptorPool,
    pub methods: Vec<Method>,
}

impl Schema {
    fn from_pool(pool: DescriptorPool) -> Self {
        let mut methods: Vec<Method> = pool
            .services()
            // The reflection service itself is plumbing, not part of anyone's API.
            .filter(|service| !service.full_name().starts_with("grpc.reflection."))
            .flat_map(|service| {
                let name = service.full_name().to_string();
                service
                    .methods()
                    .map(|method| Method {
                        service: name.clone(),
                        name: method.name().to_string(),
                        client_streaming: method.is_client_streaming(),
                        server_streaming: method.is_server_streaming(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        methods.sort_by(|a, b| (a.service.as_str(), a.name.as_str()).cmp(&(b.service.as_str(), b.name.as_str())));
        Self { pool, methods }
    }

    pub fn method(&self, path: &str) -> Option<MethodDescriptor> {
        let (service, method) = path.trim_start_matches('/').split_once('/')?;
        self.pool
            .get_service_by_name(service)?
            .methods()
            .find(|candidate| candidate.name() == method)
    }

    /// A skeleton message for a method's input, so the editor starts with something to edit
    /// rather than an empty box.
    pub fn example_for(&self, path: &str) -> Option<String> {
        let method = self.method(path)?;
        Some(example_message(&method.input(), 0))
    }
}

/// A JSON object with every field of `message`, filled with a value of the right shape.
fn example_message(message: &MessageDescriptor, depth: usize) -> String {
    if depth > 3 {
        return "{}".into();
    }
    let indent = "  ".repeat(depth + 1);
    let fields: Vec<String> = message
        .fields()
        .map(|field| {
            let value = if field.is_list() {
                format!("[{}]", example_value(&field, depth))
            } else if field.is_map() {
                "{}".to_string()
            } else {
                example_value(&field, depth)
            };
            format!("{indent}\"{}\": {value}", field.json_name())
        })
        .collect();
    if fields.is_empty() {
        return "{}".into();
    }
    format!("{{\n{}\n{}}}", fields.join(",\n"), "  ".repeat(depth))
}

fn example_value(field: &prost_reflect::FieldDescriptor, depth: usize) -> String {
    use prost_reflect::Kind;
    match field.kind() {
        Kind::Message(message) => example_message(&message, depth + 1),
        Kind::Enum(enumeration) => enumeration
            .values()
            .next()
            .map(|value| format!("\"{}\"", value.name()))
            .unwrap_or_else(|| "0".into()),
        Kind::String => "\"\"".into(),
        Kind::Bool => "false".into(),
        Kind::Bytes => "\"\"".into(),
        Kind::Double | Kind::Float => "0.0".into(),
        _ => "0".into(),
    }
}

/// Compiles `.proto` files, resolving imports against their own folders and `include`.
pub fn schema_from_protos(files: &[PathBuf], include: &[PathBuf]) -> Result<Schema> {
    if files.is_empty() {
        bail!("no .proto files to read");
    }
    let mut includes: Vec<PathBuf> = include.to_vec();
    for file in files {
        if let Some(parent) = file.parent() {
            let parent = parent.to_path_buf();
            if !includes.contains(&parent) {
                includes.push(parent);
            }
        }
    }
    let descriptors = protox::compile(files, &includes).context("reading the .proto files")?;
    let pool = DescriptorPool::from_file_descriptor_set(descriptors).context("understanding the .proto files")?;
    Ok(Schema::from_pool(pool))
}

/// Asks a server what it serves, through the standard reflection service. Tries the v1
/// service first and falls back to v1alpha, which plenty of servers still speak.
pub async fn schema_from_reflection(endpoint: &str, options: &CallOptions) -> Result<Schema> {
    let v1 = match reflect(endpoint, "grpc.reflection.v1.ServerReflection", options).await {
        Ok(schema) => return Ok(schema),
        Err(e) => e,
    };
    // Only an old server is worth a second try; any other failure is the one worth reporting.
    if !format!("{v1:#}").contains("not implemented") {
        return Err(v1);
    }
    reflect(endpoint, "grpc.reflection.v1alpha.ServerReflection", options)
        .await
        .map_err(|_| v1.context("this server doesn't offer reflection"))
}

/// How a call is made: the address, TLS, and what to send with it.
#[derive(Clone, Debug, Default)]
pub struct CallOptions {
    /// Metadata, which is what gRPC calls headers.
    pub metadata: Vec<(String, String)>,
    pub timeout_secs: u64,
    /// Skip certificate checks, from the request's settings.
    pub accept_invalid_certs: bool,
    /// A PEM bundle to trust instead of the system roots.
    pub ca_pem: Option<Vec<u8>>,
}

/// What came back from a call: one message per response, and the trailers.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A response message, as JSON.
    Message(String),
    /// The call finished: the status code, its message, and any trailing metadata.
    Finished {
        code: i32,
        status: String,
        message: String,
        metadata: Vec<(String, String)>,
    },
}

mod codec {
    //! A tonic codec that speaks `DynamicMessage`, so calls need no generated code.

    use prost::Message as _;
    use prost_reflect::{DynamicMessage, MessageDescriptor};
    use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};

    /// A codec only ever decodes one of the two message types: the responses on the client
    /// side, the requests on the server side. Which one is the caller's to say.
    #[derive(Clone)]
    pub struct Dynamic {
        pub decode: MessageDescriptor,
    }

    pub struct DynamicEncoder;
    pub struct DynamicDecoder(MessageDescriptor);

    impl Codec for Dynamic {
        type Encode = DynamicMessage;
        type Decode = DynamicMessage;
        type Encoder = DynamicEncoder;
        type Decoder = DynamicDecoder;

        fn encoder(&mut self) -> Self::Encoder {
            DynamicEncoder
        }

        fn decoder(&mut self) -> Self::Decoder {
            DynamicDecoder(self.decode.clone())
        }
    }

    impl Encoder for DynamicEncoder {
        type Item = DynamicMessage;
        type Error = tonic::Status;

        fn encode(&mut self, item: Self::Item, buf: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
            item.encode(buf)
                .map_err(|e| tonic::Status::internal(format!("could not encode the message: {e}")))
        }
    }

    impl Decoder for DynamicDecoder {
        type Item = DynamicMessage;
        type Error = tonic::Status;

        fn decode(&mut self, buf: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
            let message = DynamicMessage::decode(self.0.clone(), buf)
                .map_err(|e| tonic::Status::internal(format!("could not read the response: {e}")))?;
            Ok(Some(message))
        }
    }
}

/// Turns JSON into a message of `descriptor`'s type.
pub fn message_from_json(descriptor: &MessageDescriptor, json: &str) -> Result<DynamicMessage> {
    let json = json.trim();
    let json = if json.is_empty() { "{}" } else { json };
    let mut deserializer = serde_json::Deserializer::from_str(json);
    let message = DynamicMessage::deserialize(descriptor.clone(), &mut deserializer)
        .with_context(|| format!("this isn't a {}", descriptor.full_name()))?;
    deserializer.end()?;
    Ok(message)
}

/// The JSON for a message, as gRPC's own JSON mapping defines it.
pub fn message_to_json(message: &DynamicMessage) -> String {
    let mut buffer = Vec::new();
    let mut serializer = serde_json::Serializer::pretty(&mut buffer);
    let options = SerializeOptions::new().stringify_64_bit_integers(false);
    match message.serialize_with_options(&mut serializer, &options) {
        Ok(()) => String::from_utf8_lossy(&buffer).into_owned(),
        Err(e) => format!("{{\"error\": \"could not read the response: {e}\"}}"),
    }
}

/// Where a call is going, and how to get there.
async fn channel(endpoint: &str, options: &CallOptions) -> Result<tonic::transport::Channel> {
    let url = normalise(endpoint);
    let mut builder = tonic::transport::Endpoint::from_shared(url.clone())
        .with_context(|| format!("{url} isn't a usable address"))?
        .connect_timeout(std::time::Duration::from_secs(options.timeout_secs.max(1)));
    if url.starts_with("https://") {
        let mut tls = tonic::transport::ClientTlsConfig::new().with_enabled_roots();
        if let Some(pem) = &options.ca_pem {
            tls = tls.ca_certificate(tonic::transport::Certificate::from_pem(pem.clone()));
        }
        // tonic has no "trust anything" switch; say so rather than pretending it worked.
        if options.accept_invalid_certs {
            bail!("gRPC calls can't skip certificate checks; trust the CA instead");
        }
        builder = builder.tls_config(tls).context("setting up TLS")?;
    }
    builder.connect().await.with_context(|| format!("connecting to {url}"))
}

/// `grpc://host:port` and a bare `host:port` both mean the same thing to a person.
fn normalise(endpoint: &str) -> String {
    let endpoint = endpoint.trim();
    match endpoint {
        _ if endpoint.starts_with("http://") || endpoint.starts_with("https://") => endpoint.to_string(),
        _ if endpoint.starts_with("grpcs://") => endpoint.replacen("grpcs://", "https://", 1),
        _ if endpoint.starts_with("grpc://") => endpoint.replacen("grpc://", "http://", 1),
        _ => format!("http://{endpoint}"),
    }
}

fn request_with<T>(body: T, options: &CallOptions) -> Result<tonic::Request<T>> {
    let mut request = tonic::Request::new(body);
    for (name, value) in &options.metadata {
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        let key = tonic::metadata::MetadataKey::from_bytes(name.as_bytes())
            .with_context(|| format!("{name} isn't a usable metadata name"))?;
        let value = value
            .parse()
            .map_err(|_| anyhow!("{value} isn't a usable metadata value"))?;
        request.metadata_mut().insert(key, value);
    }
    if options.timeout_secs > 0 {
        request.set_timeout(std::time::Duration::from_secs(options.timeout_secs));
    }
    Ok(request)
}

fn finished(status: &tonic::Status) -> Event {
    Event::Finished {
        code: status.code() as i32,
        status: format!("{:?}", status.code()),
        message: status.message().to_string(),
        metadata: status
            .metadata()
            .iter()
            .filter_map(|entry| match entry {
                tonic::metadata::KeyAndValueRef::Ascii(name, value) => {
                    Some((name.to_string(), value.to_str().ok()?.to_string()))
                }
                tonic::metadata::KeyAndValueRef::Binary(name, _) => Some((name.to_string(), "<binary>".into())),
            })
            .collect(),
    }
}

/// Makes a call and reports what comes back. `messages` is one JSON message for a plain
/// call, or several for a client-streaming one. `on_event` is called as events arrive.
pub async fn call(
    endpoint: &str,
    schema: &Schema,
    path: &str,
    messages: &[String],
    options: &CallOptions,
    mut on_event: impl FnMut(Event),
) -> Result<()> {
    let method = schema
        .method(path)
        .with_context(|| format!("{path} isn't a method this server has"))?;
    let codec = codec::Dynamic {
        decode: method.output(),
    };
    let outgoing: Vec<DynamicMessage> = messages
        .iter()
        .map(|json| message_from_json(&method.input(), json))
        .collect::<Result<_>>()?;
    if outgoing.is_empty() && !method.is_client_streaming() {
        bail!("this call needs a message");
    }

    let channel = channel(endpoint, options).await?;
    let mut client = tonic::client::Grpc::new(channel);
    client.ready().await.map_err(|e| anyhow!("{e}"))?;
    let route = tonic::codegen::http::uri::PathAndQuery::from_maybe_shared(method_path(&method))
        .context("building the call's path")?;

    // Four shapes of call, from the two streaming flags.
    let outcome = match (method.is_client_streaming(), method.is_server_streaming()) {
        (false, false) => {
            let request = request_with(outgoing.into_iter().next().expect("checked above"), options)?;
            match client.unary(request, route, codec).await {
                Ok(response) => {
                    on_event(Event::Message(message_to_json(response.get_ref())));
                    Ok(())
                }
                Err(status) => Err(status),
            }
        }
        (false, true) => {
            let request = request_with(outgoing.into_iter().next().expect("checked above"), options)?;
            match client.server_streaming(request, route, codec).await {
                Ok(response) => drain(response.into_inner(), &mut on_event).await,
                Err(status) => Err(status),
            }
        }
        (true, false) => {
            let request = request_with(tokio_stream::iter(outgoing), options)?;
            match client.client_streaming(request, route, codec).await {
                Ok(response) => {
                    on_event(Event::Message(message_to_json(response.get_ref())));
                    Ok(())
                }
                Err(status) => Err(status),
            }
        }
        (true, true) => {
            let request = request_with(tokio_stream::iter(outgoing), options)?;
            match client.streaming(request, route, codec).await {
                Ok(response) => drain(response.into_inner(), &mut on_event).await,
                Err(status) => Err(status),
            }
        }
    };

    match outcome {
        Ok(()) => on_event(Event::Finished {
            code: 0,
            status: "Ok".into(),
            message: String::new(),
            metadata: Vec::new(),
        }),
        Err(status) => on_event(finished(&status)),
    }
    Ok(())
}

fn method_path(method: &MethodDescriptor) -> String {
    format!("/{}/{}", method.parent_service().full_name(), method.name())
}

async fn drain(
    mut stream: tonic::Streaming<DynamicMessage>,
    on_event: &mut impl FnMut(Event),
) -> std::result::Result<(), tonic::Status> {
    loop {
        match stream.message().await {
            Ok(Some(message)) => on_event(Event::Message(message_to_json(&message))),
            Ok(None) => return Ok(()),
            Err(status) => return Err(status),
        }
    }
}

// MARK: reflection

/// The reflection service is itself a gRPC service, so it is called the same dynamic way:
/// with a descriptor pool that holds the reflection protocol itself.
async fn reflect(endpoint: &str, service: &str, options: &CallOptions) -> Result<Schema> {
    let pool = reflection_pool(service)?;
    let request_type = pool
        .get_message_by_name(&format!(
            "{}.ServerReflectionRequest",
            service.rsplit_once('.').unwrap().0
        ))
        .context("the reflection protocol's request type")?;
    let response_type = pool
        .get_message_by_name(&format!(
            "{}.ServerReflectionResponse",
            service.rsplit_once('.').unwrap().0
        ))
        .context("the reflection protocol's response type")?;

    let ask = |field: &str, value: &str| -> Result<DynamicMessage> {
        message_from_json(&request_type, &format!("{{\"{field}\": \"{value}\"}}"))
    };

    let channel = channel(endpoint, options).await?;
    let mut client = tonic::client::Grpc::new(channel);
    client.ready().await.map_err(|e| anyhow!("{e}"))?;
    let route = tonic::codegen::http::uri::PathAndQuery::from_maybe_shared(format!("/{service}/ServerReflectionInfo"))
        .context("building the reflection path")?;
    let codec = codec::Dynamic {
        decode: response_type.clone(),
    };

    // One call lists the services, the next asks for each one's file.
    let listed = one_reflection(
        &mut client,
        route.clone(),
        codec.clone(),
        ask("listServices", "*")?,
        options,
    )
    .await?;
    let services: Vec<String> = listed
        .get("listServicesResponse")
        .and_then(|response| response.get("service"))
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|service| service.get("name")?.as_str().map(str::to_string))
        .collect();
    if services.is_empty() {
        bail!("this server's reflection listed no services");
    }

    let mut files = prost_types::FileDescriptorSet { file: Vec::new() };
    let mut seen = std::collections::HashSet::new();
    for service in services.iter().filter(|name| !name.starts_with("grpc.reflection.")) {
        let response = one_reflection(
            &mut client,
            route.clone(),
            codec.clone(),
            ask("fileContainingSymbol", service)?,
            options,
        )
        .await?;
        let descriptors = response
            .get("fileDescriptorResponse")
            .and_then(|value| value.get("fileDescriptorProto"))
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        for descriptor in descriptors {
            let Some(encoded) = descriptor.as_str().and_then(crate::encoding::base64_decode) else {
                continue;
            };
            let file = prost_types::FileDescriptorProto::decode(encoded.as_slice())
                .context("reading a file descriptor from the server")?;
            if seen.insert(file.name.clone().unwrap_or_default()) {
                files.file.push(file);
            }
        }
    }
    if files.file.is_empty() {
        bail!("this server's reflection returned no descriptors");
    }
    // Dependencies come back with the files that need them, but not always in order.
    files.file.sort_by_key(|file| file.dependency.len());
    let pool = DescriptorPool::from_file_descriptor_set(files).context("understanding what the server described")?;
    Ok(Schema::from_pool(pool))
}

/// One question and its first answer on the reflection stream.
async fn one_reflection(
    client: &mut tonic::client::Grpc<tonic::transport::Channel>,
    route: tonic::codegen::http::uri::PathAndQuery,
    codec: codec::Dynamic,
    question: DynamicMessage,
    options: &CallOptions,
) -> Result<serde_json::Value> {
    // Each call on the same client waits its turn.
    client.ready().await.map_err(|e| anyhow!("{e}"))?;
    let request = request_with(tokio_stream::iter(vec![question]), options)?;
    let response = client
        .streaming(request, route, codec)
        .await
        .map_err(|status| anyhow!("{status}"))?;
    let mut stream = response.into_inner();
    let message = stream
        .message()
        .await
        .map_err(|status| anyhow!("{status}"))?
        .context("the server said nothing")?;
    let json = message_to_json(&message);
    let value: serde_json::Value = serde_json::from_str(&json).context("reading the reflection answer")?;
    if let Some(error) = value.get("errorResponse").and_then(|error| error.get("errorMessage")) {
        bail!("{}", error.as_str().unwrap_or_default());
    }
    Ok(value)
}

/// The reflection protocol, compiled from its own definition so that talking to a server
/// needs nothing generated ahead of time.
fn reflection_pool(service: &str) -> Result<DescriptorPool> {
    let package = service
        .rsplit_once('.')
        .map(|(package, _)| package)
        .unwrap_or("grpc.reflection.v1");
    let proto = REFLECTION_PROTO.replace("PACKAGE", package);
    let dir = std::env::temp_dir().join(format!("courier-reflection-{}", crate::response_cache::fnv1a(package)));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("reflection.proto");
    std::fs::write(&path, proto)?;
    let descriptors = protox::compile([&path], [&dir]).context("reading the reflection protocol")?;
    DescriptorPool::from_file_descriptor_set(descriptors).context("understanding the reflection protocol")
}

/// The part of the reflection protocol Courier uses. Written out rather than generated, so
/// the same code serves both the v1 and v1alpha packages.
const REFLECTION_PROTO: &str = r#"
syntax = "proto3";
package PACKAGE;

service ServerReflection {
  rpc ServerReflectionInfo(stream ServerReflectionRequest) returns (stream ServerReflectionResponse);
}

message ServerReflectionRequest {
  string host = 1;
  oneof message_request {
    string file_by_filename = 3;
    string file_containing_symbol = 4;
    string list_services = 7;
  }
}

message ServerReflectionResponse {
  string valid_host = 1;
  ServerReflectionRequest original_request = 2;
  oneof message_response {
    FileDescriptorResponse file_descriptor_response = 4;
    ListServiceResponse list_services_response = 6;
    ErrorResponse error_response = 7;
  }
}

message FileDescriptorResponse { repeated bytes file_descriptor_proto = 1; }
message ListServiceResponse { repeated ServiceResponse service = 1; }
message ServiceResponse { string name = 1; }
message ErrorResponse { int32 error_code = 1; string error_message = 2; }
"#;

/// Reads `.proto` paths from a request's settings, relative to the project.
pub fn proto_paths(files: &str, base: &Path) -> Vec<PathBuf> {
    files
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let path = PathBuf::from(line);
            if path.is_absolute() { path } else { base.join(path) }
        })
        .collect()
}

/// A real gRPC server, built the same dynamic way Courier calls one, so that calls can be
/// tested end to end here and in the app.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::*;

    /// The service the test server offers.
    pub const PROTO: &str = r#"
syntax = "proto3";
package pets;

service PetService {
  rpc GetPet(GetPetRequest) returns (Pet);
  rpc ListPets(ListPetsRequest) returns (stream Pet);
  rpc AddPets(stream Pet) returns (AddPetsSummary);
  rpc Watch(stream WatchRequest) returns (stream Pet);
}

message GetPetRequest { string id = 1; bool include_toys = 2; }
message ListPetsRequest { int32 limit = 1; }
message WatchRequest { string id = 1; }
message AddPetsSummary { int32 added = 1; }
message Pet {
  string id = 1;
  string name = 2;
  Kind kind = 3;
  repeated string toys = 4;
  enum Kind { UNKNOWN = 0; DOG = 1; CAT = 2; }
}
"#;

    /// A real server built the same dynamic way, so a call is tested end to end rather than
    /// against a stand-in.
    mod server {
        use super::super::*;
        use prost_reflect::Value;
        use std::task::{Context, Poll};
        use tonic::codegen::http;

        #[derive(Clone)]
        pub struct Pets {
            pub schema: Schema,
        }

        impl Pets {
            fn pet(&self, id: &str, name: &str) -> DynamicMessage {
                let descriptor = self.schema.pool.get_message_by_name("pets.Pet").unwrap();
                let mut pet = DynamicMessage::new(descriptor);
                pet.set_field_by_name("id", Value::String(id.into()));
                pet.set_field_by_name("name", Value::String(name.into()));
                pet
            }

            fn codec(&self, path: &str) -> codec::Dynamic {
                let method = self.schema.method(path).unwrap();
                codec::Dynamic { decode: method.input() }
            }
        }

        impl<B> tower_service::Service<http::Request<B>> for Pets
        where
            B: http_body::Body<Data = prost::bytes::Bytes> + Send + 'static,
            B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send,
        {
            type Response = http::Response<tonic::body::Body>;
            type Error = std::convert::Infallible;
            type Future = std::pin::Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, request: http::Request<B>) -> Self::Future {
                let this = self.clone();
                let path = request.uri().path().to_string();
                Box::pin(async move {
                    let codec = this.codec(&path);
                    let mut grpc = tonic::server::Grpc::new(codec);
                    let response = match path.as_str() {
                        "/pets.PetService/GetPet" => {
                            struct Get(Pets);
                            impl tonic::server::UnaryService<DynamicMessage> for Get {
                                type Response = DynamicMessage;
                                type Future = std::pin::Pin<
                                    Box<
                                        dyn Future<Output = Result<tonic::Response<DynamicMessage>, tonic::Status>>
                                            + Send,
                                    >,
                                >;
                                fn call(&mut self, request: tonic::Request<DynamicMessage>) -> Self::Future {
                                    let pets = self.0.clone();
                                    Box::pin(async move {
                                        let id = request
                                            .get_ref()
                                            .get_field_by_name("id")
                                            .map(|value| value.as_str().unwrap_or_default().to_string())
                                            .unwrap_or_default();
                                        if id.is_empty() {
                                            return Err(tonic::Status::invalid_argument("a pet needs an id"));
                                        }
                                        Ok(tonic::Response::new(pets.pet(&id, "Rex")))
                                    })
                                }
                            }
                            grpc.unary(Get(this.clone()), request).await
                        }
                        "/pets.PetService/ListPets" => {
                            struct List(Pets);
                            impl tonic::server::ServerStreamingService<DynamicMessage> for List {
                                type Response = DynamicMessage;
                                type ResponseStream =
                                    tokio_stream::Iter<std::vec::IntoIter<Result<DynamicMessage, tonic::Status>>>;
                                type Future = std::pin::Pin<
                                    Box<
                                        dyn Future<
                                                Output = Result<tonic::Response<Self::ResponseStream>, tonic::Status>,
                                            > + Send,
                                    >,
                                >;
                                fn call(&mut self, _: tonic::Request<DynamicMessage>) -> Self::Future {
                                    let pets = self.0.clone();
                                    Box::pin(async move {
                                        let messages = vec![
                                            Ok(pets.pet("1", "Rex")),
                                            Ok(pets.pet("2", "Tom")),
                                            Ok(pets.pet("3", "Bo")),
                                        ];
                                        Ok(tonic::Response::new(tokio_stream::iter(messages)))
                                    })
                                }
                            }
                            grpc.server_streaming(List(this.clone()), request).await
                        }
                        "/pets.PetService/AddPets" => {
                            struct Add(Pets);
                            impl tonic::server::ClientStreamingService<DynamicMessage> for Add {
                                type Response = DynamicMessage;
                                type Future = std::pin::Pin<
                                    Box<
                                        dyn Future<Output = Result<tonic::Response<DynamicMessage>, tonic::Status>>
                                            + Send,
                                    >,
                                >;
                                fn call(
                                    &mut self,
                                    request: tonic::Request<tonic::Streaming<DynamicMessage>>,
                                ) -> Self::Future {
                                    let pets = self.0.clone();
                                    Box::pin(async move {
                                        let mut stream = request.into_inner();
                                        let mut added = 0;
                                        while stream.message().await?.is_some() {
                                            added += 1;
                                        }
                                        let descriptor =
                                            pets.schema.pool.get_message_by_name("pets.AddPetsSummary").unwrap();
                                        let mut summary = DynamicMessage::new(descriptor);
                                        summary.set_field_by_name("added", Value::I32(added));
                                        Ok(tonic::Response::new(summary))
                                    })
                                }
                            }
                            grpc.client_streaming(Add(this.clone()), request).await
                        }
                        "/pets.PetService/Watch" => {
                            struct Watch(Pets);
                            impl tonic::server::StreamingService<DynamicMessage> for Watch {
                                type Response = DynamicMessage;
                                type ResponseStream =
                                    tokio_stream::Iter<std::vec::IntoIter<Result<DynamicMessage, tonic::Status>>>;
                                type Future = std::pin::Pin<
                                    Box<
                                        dyn Future<
                                                Output = Result<tonic::Response<Self::ResponseStream>, tonic::Status>,
                                            > + Send,
                                    >,
                                >;
                                fn call(
                                    &mut self,
                                    request: tonic::Request<tonic::Streaming<DynamicMessage>>,
                                ) -> Self::Future {
                                    let pets = self.0.clone();
                                    Box::pin(async move {
                                        let mut stream = request.into_inner();
                                        let mut replies = Vec::new();
                                        while let Some(message) = stream.message().await? {
                                            let id = message
                                                .get_field_by_name("id")
                                                .map(|value| value.as_str().unwrap_or_default().to_string())
                                                .unwrap_or_default();
                                            replies.push(Ok(pets.pet(&id, &format!("watched {id}"))));
                                        }
                                        Ok(tonic::Response::new(tokio_stream::iter(replies)))
                                    })
                                }
                            }
                            grpc.streaming(Watch(this.clone()), request).await
                        }
                        _ => return Ok(tonic::Status::unimplemented(path).into_http()),
                    };
                    Ok(response)
                })
            }
        }

        impl tonic::server::NamedService for Pets {
            const NAME: &'static str = "pets.PetService";
        }
    }

    /// Starts the server on a free port of `runtime`, with reflection turned on. Returns
    /// the address and the schema it serves.
    pub fn serve(runtime: &tokio::runtime::Runtime) -> (String, Schema) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("pets.proto");
        std::fs::write(&path, PROTO).expect("writing the proto");
        let descriptors = protox::compile([&path], [dir.path()]).expect("compiling the proto");
        let pool = DescriptorPool::from_file_descriptor_set(descriptors.clone()).expect("a descriptor pool");
        let schema = Schema::from_pool(pool);

        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("a free port");
        let port = listener.local_addr().expect("the port").port();
        let reflection = tonic_reflection::server::Builder::configure()
            .register_file_descriptor_set(descriptors)
            .build_v1()
            .expect("the reflection service");
        let pets = server::Pets { schema: schema.clone() };
        runtime.spawn(async move {
            tonic::transport::Server::builder()
                .add_service(pets)
                .add_service(reflection)
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .expect("serving");
        });
        // The descriptors outlive the directory they came from.
        std::mem::forget(dir);
        (format!("grpc://127.0.0.1:{port}"), schema)
    }

    /// The server with its own runtime, for callers that just want something to call.
    /// It runs until this is dropped.
    pub struct PetServer {
        pub endpoint: String,
        _runtime: tokio::runtime::Runtime,
    }

    impl PetServer {
        pub fn start() -> Self {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("a runtime");
            let (endpoint, _schema) = serve(&runtime);
            Self {
                endpoint,
                _runtime: runtime,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> (tempfile::TempDir, Schema) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pets.proto");
        std::fs::write(&path, PROTO).unwrap();
        let schema = schema_from_protos(&[path], &[]).unwrap();
        (dir, schema)
    }

    #[test]
    fn reads_methods_from_proto_files() {
        let (_dir, schema) = schema();
        let names: Vec<String> = schema.methods.iter().map(|method| method.label()).collect();
        assert_eq!(
            names,
            [
                "PetService / AddPets",
                "PetService / GetPet",
                "PetService / ListPets",
                "PetService / Watch"
            ]
        );
        let watch = schema.methods.iter().find(|m| m.name == "Watch").unwrap();
        assert!(watch.client_streaming && watch.server_streaming, "both ways stream");
        assert_eq!(watch.path(), "/pets.PetService/Watch");
        let get = schema.methods.iter().find(|m| m.name == "GetPet").unwrap();
        assert!(!get.streaming(), "a plain call");
    }

    #[test]
    fn offers_an_example_message_to_start_from() {
        let (_dir, schema) = schema();
        let example = schema.example_for("/pets.PetService/GetPet").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&example).unwrap();
        assert_eq!(parsed["id"], "");
        assert_eq!(parsed["includeToys"], false, "fields use their JSON names");

        // A message with an enum and a list of strings still parses as JSON.
        let pet = schema.method("/pets.PetService/AddPets").unwrap().input();
        let example: serde_json::Value = serde_json::from_str(&example_message(&pet, 0)).unwrap();
        assert_eq!(example["kind"], "UNKNOWN");
        assert_eq!(example["toys"], serde_json::json!([""]));
    }

    #[test]
    fn converts_between_json_and_messages() {
        let (_dir, schema) = schema();
        let pet = schema.method("/pets.PetService/AddPets").unwrap().input();
        let message = message_from_json(&pet, r#"{"id":"7","name":"Rex","kind":"DOG","toys":["ball"]}"#).unwrap();
        let json: serde_json::Value = serde_json::from_str(&message_to_json(&message)).unwrap();
        assert_eq!(json["name"], "Rex");
        assert_eq!(json["kind"], "DOG");
        assert_eq!(json["toys"][0], "ball");

        // An empty message is a valid one; a wrong field is not.
        assert!(message_from_json(&pet, "").is_ok());
        let error = message_from_json(&pet, r#"{"nope": 1}"#).unwrap_err().to_string();
        assert!(error.contains("pets.Pet"), "{error}");
    }

    use crate::grpc::test_support::{PROTO, serve};

    fn collect(events: &std::sync::Arc<std::sync::Mutex<Vec<Event>>>) -> Vec<Event> {
        events.lock().unwrap().clone()
    }

    #[test]
    fn calls_a_real_server_every_way_a_method_can_stream() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (endpoint, schema) = serve(&runtime);
        let options = CallOptions {
            metadata: vec![("x-tenant".into(), "acme".into())],
            timeout_secs: 5,
            ..CallOptions::default()
        };
        let run = |path: &str, messages: Vec<String>| {
            let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorded = events.clone();
            runtime
                .block_on(call(&endpoint, &schema, path, &messages, &options, move |event| {
                    recorded.lock().unwrap().push(event)
                }))
                .unwrap();
            collect(&events)
        };

        // Unary.
        let events = run("/pets.PetService/GetPet", vec![r#"{"id":"7"}"#.into()]);
        assert!(
            matches!(&events[0], Event::Message(json) if json.contains("\"name\": \"Rex\"")),
            "{events:?}"
        );
        assert!(matches!(&events[1], Event::Finished { code: 0, .. }), "{events:?}");

        // Server streaming: three messages, then the status.
        let events = run("/pets.PetService/ListPets", vec!["{}".into()]);
        assert_eq!(events.len(), 4, "three pets and a status: {events:?}");
        assert!(matches!(events.last(), Some(Event::Finished { code: 0, .. })));

        // Client streaming: many messages, one answer.
        let events = run(
            "/pets.PetService/AddPets",
            vec![r#"{"id":"1"}"#.into(), r#"{"id":"2"}"#.into()],
        );
        assert!(
            matches!(&events[0], Event::Message(json) if json.contains("\"added\": 2")),
            "{events:?}"
        );

        // Both ways at once.
        let events = run(
            "/pets.PetService/Watch",
            vec![r#"{"id":"a"}"#.into(), r#"{"id":"b"}"#.into()],
        );
        assert_eq!(events.len(), 3, "one reply per request, then the status: {events:?}");

        // A refused call reports the server's own status and message.
        let events = run("/pets.PetService/GetPet", vec!["{}".into()]);
        match &events[0] {
            Event::Finished {
                code, status, message, ..
            } => {
                assert_eq!(*code, tonic::Code::InvalidArgument as i32);
                assert_eq!(status, "InvalidArgument");
                assert_eq!(message, "a pet needs an id");
            }
            other => panic!("expected a status: {other:?}"),
        }
    }

    #[test]
    fn asks_a_server_what_it_serves() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (endpoint, _schema) = serve(&runtime);
        let options = CallOptions {
            timeout_secs: 5,
            ..CallOptions::default()
        };
        let schema = runtime
            .block_on(schema_from_reflection(&endpoint, &options))
            .expect("the server offers reflection");
        let names: Vec<String> = schema.methods.iter().map(|method| method.label()).collect();
        assert_eq!(
            names,
            [
                "PetService / AddPets",
                "PetService / GetPet",
                "PetService / ListPets",
                "PetService / Watch"
            ],
            "the same methods the .proto describes"
        );
        // And a call works from a reflected schema, with no .proto in sight.
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = events.clone();
        runtime
            .block_on(call(
                &endpoint,
                &schema,
                "/pets.PetService/GetPet",
                &[r#"{"id":"9"}"#.into()],
                &options,
                move |event| recorded.lock().unwrap().push(event),
            ))
            .unwrap();
        assert!(
            matches!(&collect(&events)[0], Event::Message(json) if json.contains("\"id\": \"9\"")),
            "{:?}",
            collect(&events)
        );
    }

    #[test]
    fn understands_the_addresses_people_type() {
        assert_eq!(normalise("grpc://localhost:50051"), "http://localhost:50051");
        assert_eq!(normalise("grpcs://api.test"), "https://api.test");
        assert_eq!(normalise("localhost:50051"), "http://localhost:50051");
        assert_eq!(normalise(" https://api.test "), "https://api.test");
    }

    #[test]
    fn resolves_proto_paths_against_the_project() {
        let base = Path::new("/projects/pets");
        let paths = proto_paths("pets.proto\n# a note\n\n/absolute/other.proto", base);
        assert_eq!(
            paths,
            [
                PathBuf::from("/projects/pets/pets.proto"),
                PathBuf::from("/absolute/other.proto")
            ]
        );
    }

    #[test]
    fn compiles_the_reflection_protocol() {
        let pool = reflection_pool("grpc.reflection.v1.ServerReflection").unwrap();
        assert!(
            pool.get_message_by_name("grpc.reflection.v1.ServerReflectionRequest")
                .is_some()
        );
        let pool = reflection_pool("grpc.reflection.v1alpha.ServerReflection").unwrap();
        assert!(
            pool.get_message_by_name("grpc.reflection.v1alpha.ServerReflectionRequest")
                .is_some(),
            "the same definition serves both packages"
        );
    }
}
