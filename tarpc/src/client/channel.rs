// Copyright 2018 Google LLC
//
// Use of this source code is governed by an MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT.

use crate::{
    client::in_flight_requests::InFlightRequests, context, trace::SpanId, ClientMessage,
    PollContext, PollIo, Request, Response, Transport,
};
use futures::{prelude::*, ready, stream::Fuse, task::*};
use log::{info, trace};
use pin_project::{pin_project, pinned_drop};
use std::{
    convert::TryFrom,
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::{mpsc, oneshot};

#[allow(dead_code)]
#[allow(clippy::no_effect)]
const CHECK_USIZE: () = {
    if std::mem::size_of::<usize>() > std::mem::size_of::<u64>() {
        // TODO: replace this with panic!() as soon as RFC 2345 gets stabilized
        ["usize is too big to fit in u64"][42];
    }
};

use super::{Config, NewClient};

/// Handles communication from the client to request dispatch.
#[derive(Debug)]
pub struct Channel<Req, Resp> {
    to_dispatch: mpsc::Sender<DispatchRequest<Req, Resp>>,
    /// Channel to send a cancel message to the dispatcher.
    cancellation: RequestCancellation,
    /// The ID to use for the next request to stage.
    next_request_id: Arc<AtomicUsize>,
}

impl<Req, Resp> Clone for Channel<Req, Resp> {
    fn clone(&self) -> Self {
        Self {
            to_dispatch: self.to_dispatch.clone(),
            cancellation: self.cancellation.clone(),
            next_request_id: self.next_request_id.clone(),
        }
    }
}

impl<Req, Resp> Channel<Req, Resp> {
    /// Sends a request to the dispatch task to forward to the server, returning a [`Future`] that
    /// resolves when the request is sent (not when the response is received).
    fn send(
        &self,
        mut ctx: context::Context,
        request: Req,
    ) -> impl Future<Output = io::Result<DispatchResponse<Resp>>> + '_ {
        // Convert the context to the call context.
        ctx.trace_context.parent_id = Some(ctx.trace_context.span_id);
        ctx.trace_context.span_id = SpanId::random(&mut rand::thread_rng());

        let (response_completion, response) = oneshot::channel();
        let cancellation = self.cancellation.clone();
        let request_id =
            u64::try_from(self.next_request_id.fetch_add(1, Ordering::Relaxed)).unwrap();

        // DispatchResponse impls Drop to cancel in-flight requests. It should be created before
        // sending out the request; otherwise, the response future could be dropped after the
        // request is sent out but before DispatchResponse is created, rendering the cancellation
        // logic inactive.
        let response = DispatchResponse {
            response,
            complete: false,
            request_id,
            cancellation,
            ctx,
        };
        async move {
            self.to_dispatch
                .send(DispatchRequest {
                    ctx,
                    request_id,
                    request,
                    response_completion,
                })
                .await
                .map_err(|mpsc::error::SendError(_)| {
                    io::Error::from(io::ErrorKind::ConnectionReset)
                })?;
            Ok(response)
        }
    }

    /// Sends a request to the dispatch task to forward to the server, returning a [`Future`] that
    /// resolves to the response.
    pub async fn call(&self, ctx: context::Context, request: Req) -> io::Result<Resp> {
        let dispatch_response = self.send(ctx, request).await?;
        dispatch_response.await
    }
}

/// A server response that is completed by request dispatch when the corresponding response
/// arrives off the wire.
#[pin_project(PinnedDrop)]
#[derive(Debug)]
struct DispatchResponse<Resp> {
    response: oneshot::Receiver<Response<Resp>>,
    ctx: context::Context,
    complete: bool,
    cancellation: RequestCancellation,
    request_id: u64,
}

impl<Resp> Future for DispatchResponse<Resp> {
    type Output = io::Result<Resp>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<Resp>> {
        let resp = ready!(self.response.poll_unpin(cx));
        self.complete = true;
        Poll::Ready(match resp {
            Ok(resp) => Ok(resp.message?),
            Err(oneshot::error::RecvError { .. }) => {
                // The oneshot is Canceled when the dispatch task ends. In that case,
                // there's nothing listening on the other side, so there's no point in
                // propagating cancellation.
                Err(io::Error::from(io::ErrorKind::ConnectionReset))
            }
        })
    }
}

// Cancels the request when dropped, if not already complete.
#[pinned_drop]
impl<Resp> PinnedDrop for DispatchResponse<Resp> {
    fn drop(mut self: Pin<&mut Self>) {
        if !self.complete {
            // The receiver needs to be closed to handle the edge case that the request has not
            // yet been received by the dispatch task. It is possible for the cancel message to
            // arrive before the request itself, in which case the request could get stuck in the
            // dispatch map forever if the server never responds (e.g. if the server dies while
            // responding). Even if the server does respond, it will have unnecessarily done work
            // for a client no longer waiting for a response. To avoid this, the dispatch task
            // checks if the receiver is closed before inserting the request in the map. By
            // closing the receiver before sending the cancel message, it is guaranteed that if the
            // dispatch task misses an early-arriving cancellation message, then it will see the
            // receiver as closed.
            self.response.close();
            let request_id = self.request_id;
            self.cancellation.cancel(request_id);
        }
    }
}

/// Returns a channel and dispatcher that manages the lifecycle of requests initiated by the
/// channel.
pub fn new<Req, Resp, C>(
    config: Config,
    transport: C,
) -> NewClient<Channel<Req, Resp>, RequestDispatch<Req, Resp, C>>
where
    C: Transport<ClientMessage<Req>, Response<Resp>>,
{
    let (to_dispatch, pending_requests) = mpsc::channel(config.pending_request_buffer);
    let (cancellation, canceled_requests) = cancellations();
    let canceled_requests = canceled_requests;

    NewClient {
        client: Channel {
            to_dispatch,
            cancellation,
            next_request_id: Arc::new(AtomicUsize::new(0)),
        },
        dispatch: RequestDispatch {
            config,
            canceled_requests,
            transport: transport.fuse(),
            in_flight_requests: InFlightRequests::default(),
            pending_requests,
        },
    }
}

/// Handles the lifecycle of requests, writing requests to the wire, managing cancellations,
/// and dispatching responses to the appropriate channel.
#[pin_project]
#[derive(Debug)]
pub struct RequestDispatch<Req, Resp, C> {
    /// Writes requests to the wire and reads responses off the wire.
    #[pin]
    transport: Fuse<C>,
    /// Requests waiting to be written to the wire.
    #[pin]
    pending_requests: mpsc::Receiver<DispatchRequest<Req, Resp>>,
    /// Requests that were dropped.
    #[pin]
    canceled_requests: CanceledRequests,
    /// Requests already written to the wire that haven't yet received responses.
    in_flight_requests: InFlightRequests<Resp>,
    /// Configures limits to prevent unlimited resource usage.
    config: Config,
}

impl<Req, Resp, C> RequestDispatch<Req, Resp, C>
where
    C: Transport<ClientMessage<Req>, Response<Resp>>,
{
    fn in_flight_requests<'a>(self: &'a mut Pin<&mut Self>) -> &'a mut InFlightRequests<Resp> {
        self.as_mut().project().in_flight_requests
    }

    fn pump_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> PollIo<()> {
        Poll::Ready(
            match ready!(self.as_mut().project().transport.poll_next(cx)?) {
                Some(response) => {
                    self.complete(response);
                    Some(Ok(()))
                }
                None => None,
            },
        )
    }

    fn pump_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> PollIo<()> {
        enum ReceiverStatus {
            NotReady,
            Closed,
        }

        let pending_requests_status = match self.as_mut().poll_next_request(cx)? {
            Poll::Ready(Some(dispatch_request)) => {
                self.as_mut().write_request(dispatch_request)?;
                return Poll::Ready(Some(Ok(())));
            }
            Poll::Ready(None) => ReceiverStatus::Closed,
            Poll::Pending => ReceiverStatus::NotReady,
        };

        let canceled_requests_status = match self.as_mut().poll_next_cancellation(cx)? {
            Poll::Ready(Some((context, request_id))) => {
                self.as_mut().write_cancel(context, request_id)?;
                return Poll::Ready(Some(Ok(())));
            }
            Poll::Ready(None) => ReceiverStatus::Closed,
            Poll::Pending => ReceiverStatus::NotReady,
        };

        // Receiving Poll::Ready(None) when polling expired requests never indicates "Closed",
        // because there can temporarily be zero in-flight rquests. Therefore, there is no need to
        // track the status like is done with pending and cancelled requests.
        if let Poll::Ready(Some(_)) = self.in_flight_requests().poll_expired(cx)? {
            // Expired requests are considered complete; there is no compelling reason to send a
            // cancellation message to the server, since it will have already exhausted its
            // allotted processing time.
            return Poll::Ready(Some(Ok(())));
        }

        match (pending_requests_status, canceled_requests_status) {
            (ReceiverStatus::Closed, ReceiverStatus::Closed) => {
                ready!(self.as_mut().project().transport.poll_flush(cx)?);
                Poll::Ready(None)
            }
            (ReceiverStatus::NotReady, _) | (_, ReceiverStatus::NotReady) => {
                // No more messages to process, so flush any messages buffered in the transport.
                ready!(self.as_mut().project().transport.poll_flush(cx)?);

                // Even if we fully-flush, we return Pending, because we have no more requests
                // or cancellations right now.
                Poll::Pending
            }
        }
    }

    /// Yields the next pending request, if one is ready to be sent.
    fn poll_next_request(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> PollIo<DispatchRequest<Req, Resp>> {
        if self.in_flight_requests().len() >= self.config.max_in_flight_requests {
            info!(
                "At in-flight request capacity ({}/{}).",
                self.in_flight_requests().len(),
                self.config.max_in_flight_requests
            );

            // No need to schedule a wakeup, because timers and responses are responsible
            // for clearing out in-flight requests.
            return Poll::Pending;
        }

        while self
            .as_mut()
            .project()
            .transport
            .poll_ready(cx)?
            .is_pending()
        {
            // We can't yield a request-to-be-sent before the transport is capable of buffering it.
            ready!(self.as_mut().project().transport.poll_flush(cx)?);
        }

        loop {
            match ready!(self.as_mut().project().pending_requests.poll_recv(cx)) {
                Some(request) => {
                    if request.response_completion.is_closed() {
                        trace!(
                            "[{}] Request canceled before being sent.",
                            request.ctx.trace_id()
                        );
                        continue;
                    }

                    return Poll::Ready(Some(Ok(request)));
                }
                None => return Poll::Ready(None),
            }
        }
    }

    /// Yields the next pending cancellation, and, if one is ready, cancels the associated request.
    fn poll_next_cancellation(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> PollIo<(context::Context, u64)> {
        while self
            .as_mut()
            .project()
            .transport
            .poll_ready(cx)?
            .is_pending()
        {
            ready!(self.as_mut().project().transport.poll_flush(cx)?);
        }

        loop {
            let cancellation = self
                .as_mut()
                .project()
                .canceled_requests
                .poll_next_unpin(cx);
            match ready!(cancellation) {
                Some(request_id) => {
                    if let Some(ctx) = self.in_flight_requests().cancel_request(request_id) {
                        return Poll::Ready(Some(Ok((ctx, request_id))));
                    }
                }
                None => return Poll::Ready(None),
            }
        }
    }

    fn write_request(
        mut self: Pin<&mut Self>,
        dispatch_request: DispatchRequest<Req, Resp>,
    ) -> io::Result<()> {
        let request_id = dispatch_request.request_id;
        let request = ClientMessage::Request(Request {
            id: request_id,
            message: dispatch_request.request,
            context: context::Context {
                deadline: dispatch_request.ctx.deadline,
                trace_context: dispatch_request.ctx.trace_context,
            },
        });
        self.as_mut().project().transport.start_send(request)?;
        self.in_flight_requests()
            .insert_request(
                request_id,
                dispatch_request.ctx,
                dispatch_request.response_completion,
            )
            .expect("Request IDs should be unique");
        Ok(())
    }

    fn write_cancel(
        mut self: Pin<&mut Self>,
        context: context::Context,
        request_id: u64,
    ) -> io::Result<()> {
        let trace_id = *context.trace_id();
        let cancel = ClientMessage::Cancel {
            trace_context: context.trace_context,
            request_id,
        };
        self.as_mut().project().transport.start_send(cancel)?;
        trace!("[{}] Cancel message sent.", trace_id);
        Ok(())
    }

    /// Sends a server response to the client task that initiated the associated request.
    fn complete(mut self: Pin<&mut Self>, response: Response<Resp>) -> bool {
        self.in_flight_requests().complete_request(response)
    }
}

impl<Req, Resp, C> Future for RequestDispatch<Req, Resp, C>
where
    C: Transport<ClientMessage<Req>, Response<Resp>>,
{
    type Output = anyhow::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<anyhow::Result<()>> {
        loop {
            match (
                self.as_mut()
                    .pump_read(cx)
                    .context("failed to read from transport")?,
                self.as_mut()
                    .pump_write(cx)
                    .context("failed to write to transport")?,
            ) {
                (Poll::Ready(None), _) => {
                    info!("Shutdown: read half closed, so shutting down.");
                    return Poll::Ready(Ok(()));
                }
                (read, Poll::Ready(None)) => {
                    if self.in_flight_requests().is_empty() {
                        info!("Shutdown: write half closed, and no requests in flight.");
                        return Poll::Ready(Ok(()));
                    }
                    info!(
                        "Shutdown: write half closed, and {} requests in flight.",
                        self.in_flight_requests().len()
                    );
                    match read {
                        Poll::Ready(Some(())) => continue,
                        _ => return Poll::Pending,
                    }
                }
                (Poll::Ready(Some(())), _) | (_, Poll::Ready(Some(()))) => {}
                _ => return Poll::Pending,
            }
        }
    }
}

/// A server-bound request sent from a [`Channel`] to request dispatch, which will then manage
/// the lifecycle of the request.
#[derive(Debug)]
struct DispatchRequest<Req, Resp> {
    pub ctx: context::Context,
    pub request_id: u64,
    pub request: Req,
    pub response_completion: oneshot::Sender<Response<Resp>>,
}

/// Sends request cancellation signals.
#[derive(Debug, Clone)]
struct RequestCancellation(mpsc::UnboundedSender<u64>);

/// A stream of IDs of requests that have been canceled.
#[derive(Debug)]
struct CanceledRequests(mpsc::UnboundedReceiver<u64>);

/// Returns a channel to send request cancellation messages.
fn cancellations() -> (RequestCancellation, CanceledRequests) {
    // Unbounded because messages are sent in the drop fn. This is fine, because it's still
    // bounded by the number of in-flight requests. Additionally, each request has a clone
    // of the sender, so the bounded channel would have the same behavior,
    // since it guarantees a slot.
    let (tx, rx) = mpsc::unbounded_channel();
    (RequestCancellation(tx), CanceledRequests(rx))
}

impl RequestCancellation {
    /// Cancels the request with ID `request_id`.
    fn cancel(&mut self, request_id: u64) {
        let _ = self.0.send(request_id);
    }
}

impl Stream for CanceledRequests {
    type Item = u64;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<u64>> {
        self.0.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        cancellations, CanceledRequests, Channel, DispatchResponse, RequestCancellation,
        RequestDispatch,
    };
    use crate::{
        client::{in_flight_requests::InFlightRequests, Config},
        context,
        transport::{self, channel::UnboundedChannel},
        ClientMessage, Response,
    };
    use futures::{prelude::*, task::*};
    use std::{pin::Pin, sync::atomic::AtomicUsize, sync::Arc};
    use tokio::sync::{mpsc, oneshot};

    #[tokio::test]
    async fn dispatch_response_cancels_on_drop() {
        let (cancellation, mut canceled_requests) = cancellations();
        let (_, response) = oneshot::channel();
        drop(DispatchResponse::<u32> {
            response,
            cancellation,
            complete: false,
            request_id: 3,
            ctx: context::current(),
        });
        // resp's drop() is run, which should send a cancel message.
        let cx = &mut Context::from_waker(&noop_waker_ref());
        assert_eq!(canceled_requests.0.poll_recv(cx), Poll::Ready(Some(3)));
    }

    #[tokio::test]
    async fn stage_request() {
        let (mut dispatch, mut channel, _server_channel) = set_up();
        let dispatch = Pin::new(&mut dispatch);
        let cx = &mut Context::from_waker(&noop_waker_ref());

        let _resp = send_request(&mut channel, "hi").await;

        let req = dispatch.poll_next_request(cx).ready();
        assert!(req.is_some());

        let req = req.unwrap();
        assert_eq!(req.request_id, 0);
        assert_eq!(req.request, "hi".to_string());
    }

    // Regression test for  https://github.com/google/tarpc/issues/220
    #[tokio::test]
    async fn stage_request_channel_dropped_doesnt_panic() {
        let (mut dispatch, mut channel, mut server_channel) = set_up();
        let mut dispatch = Pin::new(&mut dispatch);
        let cx = &mut Context::from_waker(&noop_waker_ref());

        let _ = send_request(&mut channel, "hi").await;
        drop(channel);

        assert!(dispatch.as_mut().poll(cx).is_ready());
        send_response(
            &mut server_channel,
            Response {
                request_id: 0,
                message: Ok("hello".into()),
            },
        )
        .await;
        dispatch.await.unwrap();
    }

    #[tokio::test]
    async fn stage_request_response_future_dropped_is_canceled_before_sending() {
        let (mut dispatch, mut channel, _server_channel) = set_up();
        let dispatch = Pin::new(&mut dispatch);
        let cx = &mut Context::from_waker(&noop_waker_ref());

        let _ = send_request(&mut channel, "hi").await;

        // Drop the channel so polling returns none if no requests are currently ready.
        drop(channel);
        // Test that a request future dropped before it's processed by dispatch will cause the request
        // to not be added to the in-flight request map.
        assert!(dispatch.poll_next_request(cx).ready().is_none());
    }

    #[tokio::test]
    async fn stage_request_response_future_dropped_is_canceled_after_sending() {
        let (mut dispatch, mut channel, _server_channel) = set_up();
        let cx = &mut Context::from_waker(&noop_waker_ref());
        let mut dispatch = Pin::new(&mut dispatch);

        let req = send_request(&mut channel, "hi").await;

        assert!(dispatch.as_mut().pump_write(cx).ready().is_some());
        assert!(!dispatch.in_flight_requests().is_empty());

        // Test that a request future dropped after it's processed by dispatch will cause the request
        // to be removed from the in-flight request map.
        drop(req);
        if let Poll::Ready(Some(_)) = dispatch.as_mut().poll_next_cancellation(cx).unwrap() {
            // ok
        } else {
            panic!("Expected request to be cancelled")
        };
        assert!(dispatch.in_flight_requests().is_empty());
    }

    #[tokio::test]
    async fn stage_request_response_closed_skipped() {
        let (mut dispatch, mut channel, _server_channel) = set_up();
        let dispatch = Pin::new(&mut dispatch);
        let cx = &mut Context::from_waker(&noop_waker_ref());

        // Test that a request future that's closed its receiver but not yet canceled its request --
        // i.e. still in `drop fn` -- will cause the request to not be added to the in-flight request
        // map.
        let mut resp = send_request(&mut channel, "hi").await;
        resp.response.close();

        assert!(dispatch.poll_next_request(cx).is_pending());
    }

    fn set_up() -> (
        RequestDispatch<String, String, UnboundedChannel<Response<String>, ClientMessage<String>>>,
        Channel<String, String>,
        UnboundedChannel<ClientMessage<String>, Response<String>>,
    ) {
        let _ = env_logger::try_init();

        let (to_dispatch, pending_requests) = mpsc::channel(1);
        let (cancel_tx, canceled_requests) = mpsc::unbounded_channel();
        let (client_channel, server_channel) = transport::channel::unbounded();

        let dispatch = RequestDispatch::<String, String, _> {
            transport: client_channel.fuse(),
            pending_requests: pending_requests,
            canceled_requests: CanceledRequests(canceled_requests),
            in_flight_requests: InFlightRequests::default(),
            config: Config::default(),
        };

        let cancellation = RequestCancellation(cancel_tx);
        let channel = Channel {
            to_dispatch,
            cancellation,
            next_request_id: Arc::new(AtomicUsize::new(0)),
        };

        (dispatch, channel, server_channel)
    }

    async fn send_request(
        channel: &mut Channel<String, String>,
        request: &str,
    ) -> DispatchResponse<String> {
        channel
            .send(context::current(), request.to_string())
            .await
            .unwrap()
    }

    async fn send_response(
        channel: &mut UnboundedChannel<ClientMessage<String>, Response<String>>,
        response: Response<String>,
    ) {
        channel.send(response).await.unwrap();
    }

    trait PollTest {
        type T;
        fn unwrap(self) -> Poll<Self::T>;
        fn ready(self) -> Self::T;
    }

    impl<T, E> PollTest for Poll<Option<Result<T, E>>>
    where
        E: ::std::fmt::Display,
    {
        type T = Option<T>;

        fn unwrap(self) -> Poll<Option<T>> {
            match self {
                Poll::Ready(Some(Ok(t))) => Poll::Ready(Some(t)),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Ready(Some(Err(e))) => panic!("{}", e.to_string()),
                Poll::Pending => Poll::Pending,
            }
        }

        fn ready(self) -> Option<T> {
            match self {
                Poll::Ready(Some(Ok(t))) => Some(t),
                Poll::Ready(None) => None,
                Poll::Ready(Some(Err(e))) => panic!("{}", e.to_string()),
                Poll::Pending => panic!("Pending"),
            }
        }
    }
}
