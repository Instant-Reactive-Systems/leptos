use crate::{
    error::{FromServerFnError, IntoAppError, ServerFnErrorErr},
    request::Req,
};
use axum::{
    body::{Body, Bytes},
    response::Response,
};
use futures::{Sink, Stream, StreamExt};
use http::{
    header::{ACCEPT, CONTENT_TYPE, REFERER},
    Request,
};
use http_body_util::BodyExt;
use std::borrow::Cow;

impl<E> Req<E> for Request<Body>
where
    E: FromServerFnError + Send,
{
    type WebsocketResponse = Response;

    fn as_query(&self) -> Option<&str> {
        self.uri().query()
    }

    fn to_content_type(&self) -> Option<Cow<'_, str>> {
        self.headers()
            .get(CONTENT_TYPE)
            .map(|h| String::from_utf8_lossy(h.as_bytes()))
    }

    fn accepts(&self) -> Option<Cow<'_, str>> {
        self.headers()
            .get(ACCEPT)
            .map(|h| String::from_utf8_lossy(h.as_bytes()))
    }

    fn referer(&self) -> Option<Cow<'_, str>> {
        self.headers()
            .get(REFERER)
            .map(|h| String::from_utf8_lossy(h.as_bytes()))
    }

    async fn try_into_bytes(self) -> Result<Bytes, E> {
        let (_parts, body) = self.into_parts();

        body.collect().await.map(|c| c.to_bytes()).map_err(|e| {
            ServerFnErrorErr::Deserialization(e.to_string()).into_app_error()
        })
    }

    async fn try_into_string(self) -> Result<String, E> {
        let bytes = self.try_into_bytes().await?;
        String::from_utf8(bytes.to_vec()).map_err(|e| {
            ServerFnErrorErr::Deserialization(e.to_string()).into_app_error()
        })
    }

    fn try_into_stream(
        self,
    ) -> Result<impl Stream<Item = Result<Bytes, E>> + Send + 'static, E> {
        Ok(self.into_body().into_data_stream().map(|chunk| {
            chunk.map_err(|e| {
                ServerFnErrorErr::Deserialization(e.to_string())
                    .into_app_error()
            })
        }))
    }

    async fn try_into_websocket(
        self,
    ) -> Result<
        (
            impl Stream<Item = Result<Bytes, E>> + Send + 'static,
            impl Sink<Result<Bytes, E>> + Send + 'static,
            Self::WebsocketResponse,
        ),
        E,
    > {
        #[cfg(not(feature = "axum"))]
        {
            Err::<
                (
                    futures::stream::Once<std::future::Ready<Result<Bytes, E>>>,
                    futures::sink::Drain<Result<Bytes, E>>,
                    Self::WebsocketResponse,
                ),
                _,
            >(E::from_server_fn_error(
                crate::ServerFnErrorErr::Response(
                    "Websocket connections not supported for Axum when the \
                     `axum` feature is not enabled on the `server_fn` crate."
                        .to_string(),
                ),
            ))
        }
        #[cfg(feature = "axum")]
        {
            use axum::extract::ws::Message;
            use axum::extract::FromRequest;
            use futures::FutureExt;

            let upgrade =
                axum::extract::ws::WebSocketUpgrade::from_request(self, &())
                    .await
                    .map_err(|err| {
                        E::from_server_fn_error(ServerFnErrorErr::Request(
                            err.to_string(),
                        ))
                    })?;
            let (mut outgoing_tx, outgoing_rx) =
                futures::channel::mpsc::channel(2048);
            let (incoming_tx, mut incoming_rx) =
                futures::channel::mpsc::channel::<Result<Bytes, E>>(2048);
            let response = upgrade
        .on_failed_upgrade({
            let mut outgoing_tx = outgoing_tx.clone();
            move |err: axum::Error| {
                _ = outgoing_tx.start_send(Err(E::from_server_fn_error(ServerFnErrorErr::Response(err.to_string()))));
            }
        })
        .on_upgrade(|mut session| async move {
            loop {
                futures::select! {
                    incoming = incoming_rx.next() => {
                        let Some(incoming) = incoming else {
                            break;
                        };
                        match incoming {
                            Ok(message) => {
                                if let Err(err) = session.send(Message::Binary(message)).await {
                                    _ = outgoing_tx.start_send(Err(E::from_server_fn_error(ServerFnErrorErr::Request(err.to_string()))));
                                }
                            }
                            Err(err) => {
                                _ = outgoing_tx.start_send(Err(err));
                            }
                        }
                    },
                    outgoing = session.recv().fuse() => {
                        let Some(outgoing) = outgoing else {
                            break;
                        };
                        match outgoing {
                            Ok(Message::Binary(bytes)) => {
                                _ = outgoing_tx
                                    .start_send(
                                        Ok(bytes),
                                    );
                            }
                            Ok(Message::Text(text)) => {
                                _ = outgoing_tx.start_send(Ok(Bytes::from(text)));
                            }
                            Ok(_other) => {}
                            Err(e) => {
                                _ = outgoing_tx.start_send(Err(E::from_server_fn_error(ServerFnErrorErr::Response(e.to_string()))));
                            }
                        }
                    }
                }
            }
            _ = session.send(Message::Close(None)).await;
        });

            Ok((outgoing_rx, incoming_tx, response))
        }
    }
}
