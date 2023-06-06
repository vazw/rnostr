use crate::{Result, Server, Setting};
use actix::Addr;
use actix_cors::Cors;
use actix_web::{
    body::MessageBody,
    dev::{ServiceFactory, ServiceRequest},
    web, App, HttpServer,
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use metrics_util::MetricKindMask;
use nostr_db::Db;
use parking_lot::RwLock;
use std::{net::ToSocketAddrs, path::Path, sync::Arc, time::Duration};

pub mod route {
    use crate::{AppData, Session};
    use actix_http::header::{ACCEPT, UPGRADE};
    use actix_web::{web, Error, HttpRequest, HttpResponse};
    use actix_web_actors::ws;

    pub async fn websocket(
        req: HttpRequest,
        stream: web::Payload,
        data: web::Data<AppData>,
    ) -> Result<HttpResponse, Error> {
        ws::start(
            Session::new(
                req.connection_info()
                    .realip_remote_addr()
                    .map(ToOwned::to_owned)
                    .unwrap_or_default(),
                data.get_ref(),
            ),
            &req,
            stream,
        )
    }

    pub async fn information(
        _req: HttpRequest,
        _stream: web::Payload,
        data: web::Data<AppData>,
    ) -> Result<HttpResponse, Error> {
        let r = data.setting.read();
        Ok(HttpResponse::Ok()
            .insert_header(("Content-Type", "application/nostr+json"))
            .body(r.render_information()?))
    }

    pub async fn index(
        req: HttpRequest,
        stream: web::Payload,
        data: web::Data<AppData>,
    ) -> Result<HttpResponse, Error> {
        let headers = req.headers();
        if headers.contains_key(UPGRADE) {
            return websocket(req, stream, data).await;
        } else if let Some(accept) = headers.get(ACCEPT) {
            if let Ok(accept) = accept.to_str() {
                if accept.contains("application/nostr+json") {
                    return information(req, stream, data).await;
                }
            }
        }

        Ok(HttpResponse::Ok().body("Hello World!"))
    }

    pub async fn metrics(
        _req: HttpRequest,
        _stream: web::Payload,
        data: web::Data<AppData>,
    ) -> Result<HttpResponse, Error> {
        Ok(HttpResponse::Ok()
            .insert_header(("Content-Type", "text/plain"))
            .body(data.prometheus_handle.render()))
    }
}

#[derive(Clone)]
pub struct AppData {
    pub server: Addr<Server>,
    pub setting: Arc<RwLock<Setting>>,
    pub prometheus_handle: PrometheusHandle,
}

impl AppData {
    pub fn create<P: AsRef<Path>>(
        db_path: Option<P>,
        prometheus_handle: PrometheusHandle,
    ) -> Result<Self> {
        let setting = Setting::default_wrapper();

        let r = setting.read();
        let path = db_path
            .map(|p| p.as_ref().to_path_buf())
            .unwrap_or(r.db.path.clone());
        drop(r);
        let db = Arc::new(Db::open(path)?);

        let server = Server::create_with(db, Arc::clone(&setting));

        Ok(Self {
            server,
            setting,
            prometheus_handle,
        })
    }
}

pub fn create_prometheus_handle() -> PrometheusHandle {
    let builder = PrometheusBuilder::new();
    builder
        .idle_timeout(MetricKindMask::ALL, Some(Duration::from_secs(10)))
        .install_recorder()
        .unwrap()
}

pub fn create_app(
    data: AppData,
) -> App<
    impl ServiceFactory<
        ServiceRequest,
        Config = (),
        Response = actix_web::dev::ServiceResponse<impl MessageBody>,
        Error = actix_web::Error,
        InitError = (),
    >,
> {
    let app = App::new();
    app.app_data(web::Data::new(data))
        .service(web::resource("/metrics").route(web::get().to(route::metrics)))
        .service(web::resource("/").route(web::get().to(route::index)))
        .wrap(
            Cors::default()
                .send_wildcard()
                .allow_any_header()
                .allow_any_origin()
                .allow_any_method()
                .max_age(86_400), // 24h
        )
}

pub async fn start_app<A: ToSocketAddrs>(addrs: A, data: AppData) -> Result<(), std::io::Error> {
    // start server actor
    // let server = Server::default().start();
    let r = data.setting.read();
    let num = if r.thread.reader == 0 {
        num_cpus::get()
    } else {
        r.thread.reader
    };
    drop(r);
    HttpServer::new(move || create_app(data.clone()))
        .workers(num)
        .bind(addrs)?
        .run()
        .await?;
    //TODO: save db
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use actix_web::{
        dev::Service,
        test::{init_service, read_body, TestRequest},
    };
    use actix_web_actors::ws;
    use anyhow::Result;
    use bytes::Bytes;
    use futures_util::{SinkExt as _, StreamExt as _};
    use lazy_static::lazy_static;
    lazy_static! {
        pub static ref PROMETHEUS_HANDLE: PrometheusHandle = create_prometheus_handle();
    }

    fn db_path(p: &str) -> Result<tempfile::TempDir> {
        Ok(tempfile::Builder::new()
            .prefix(&format!("nostr-relay-test-db-{}", p))
            .tempdir()?)
    }

    #[actix_rt::test]
    async fn relay_info() -> Result<()> {
        let data = AppData::create(Some(db_path("")?), PROMETHEUS_HANDLE.clone())?;
        let app = init_service(create_app(data)).await;
        let req = TestRequest::with_uri("/")
            .insert_header(("Accept", "application/nostr+json"))
            .to_request();
        let res = app.call(req).await.unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(
            res.headers().get(actix_http::header::CONTENT_TYPE).unwrap(),
            "application/nostr+json"
        );

        Ok(())
    }

    #[actix_rt::test]
    async fn metrics() -> Result<()> {
        let data = AppData::create(Some(db_path("")?), PROMETHEUS_HANDLE.clone())?;
        metrics::increment_counter!("test_metric");

        let app = init_service(create_app(data)).await;
        let req = TestRequest::with_uri("/metrics").to_request();
        let res = app.call(req).await.unwrap();
        assert_eq!(res.status(), 200);

        let result = read_body(res).await;
        let result = String::from_utf8(result.to_vec())?;
        assert!(result.contains("test_metric"));
        Ok(())
    }

    #[actix_rt::test]
    async fn connect_ws() -> Result<()> {
        let data = AppData::create(Some(db_path("")?), PROMETHEUS_HANDLE.clone())?;

        let mut srv = actix_test::start(move || create_app(data.clone()));

        // client service
        let mut framed = srv.ws_at("/").await.unwrap();

        framed.send(ws::Message::Ping("text".into())).await?;
        let item = framed.next().await.unwrap()?;
        assert_eq!(item, ws::Frame::Pong(Bytes::copy_from_slice(b"text")));

        framed
            .send(ws::Message::Close(Some(ws::CloseCode::Normal.into())))
            .await?;
        let item = framed.next().await.unwrap()?;
        assert_eq!(item, ws::Frame::Close(Some(ws::CloseCode::Normal.into())));
        Ok(())
    }
}