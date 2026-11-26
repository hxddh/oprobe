//! The gate every HTTP request of a run passes before it reaches the wire: budgets are
//! enforced here, request by request and chunk by chunk, and a cancelled run stops here.
//!
//! [`Gate::wrap`] puts a transport behind the gate:
//!
//! - **Before a request is sent** it is admitted by the [`Guard`] (counted with its body, so a
//!   retry or a re-sent request counts again). A request the budget has no room for, and any
//!   request of a cancelled run, is refused: it never reaches the target.
//! - **While a request is in flight** a cancellation (Ctrl-C, a signal, Stop, a budget) cuts
//!   it: the operation it belongs to fails at once instead of running to its end.
//! - **Response bodies** are counted as their chunks arrive, and stop at the chunk that
//!   exceeds the byte budget or follows the cancellation.
//!
//! Two kinds of request are treated differently, so that stopping never leaves a multipart
//! upload behind:
//!
//! - *Aborting a multipart upload* is cleanup: never counted, refused or cut.
//! - *Creating a multipart upload* is counted and can be refused, but once sent it is not cut
//!   with the others: its response carries the upload id, without which the upload could not
//!   be aborted. A cancelled run gives it [`CREATE_GRACE`] to answer. One that has not by then
//!   (a stalled service) is given up, so that the run can go on to its cleanup, and reported
//!   with the uploads that could not be aborted ([`Aborts::failure`]): if the service did
//!   create it, nothing here knows its id.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use opendal::raw::ServiceOperation;
use opendal::{Buffer, Error, ErrorKind, HttpBody, HttpTransport};

use super::Cancel;
use super::upload::Aborts;
use crate::guard::{BudgetBreach, Guard};
use crate::trace::Api;

/// How long a `CreateMultipartUpload` in flight may still take to answer once the run is
/// cancelled.
pub const CREATE_GRACE: Duration = Duration::from_secs(10);

/// What stops the requests of one measurement: its budgets and its cancellation.
#[derive(Clone)]
pub struct Gate {
    guard: Option<Arc<Guard>>,
    cancel: Cancel,
    /// Where a created upload that never answered is reported.
    aborts: Aborts,
}

/// The error of a request stopped because the run was cancelled. Persistent: no layer retries
/// it.
fn cancelled() -> Error {
    Error::new(ErrorKind::Unexpected, "the run was cancelled")
}

/// The error of a request stopped by a budget. Persistent: no layer retries it.
fn over_budget(b: &BudgetBreach) -> Error {
    Error::new(ErrorKind::Unexpected, b.message())
}

impl Gate {
    pub fn new(guard: Option<Arc<Guard>>, cancel: Cancel, aborts: Aborts) -> Self {
        Self {
            guard,
            cancel,
            aborts,
        }
    }

    /// `transport` behind this gate.
    pub fn wrap<T: HttpTransport>(&self, transport: T) -> Gated<T> {
        Gated {
            inner: transport,
            gate: self.clone(),
        }
    }

    /// May a request with a body of `body_bytes` be sent? Counts it when it may.
    fn admit(&self, body_bytes: u64) -> Result<(), Error> {
        if self.cancel.is_cancelled() {
            return Err(cancelled());
        }
        match &self.guard {
            Some(g) => g.admit(body_bytes).map_err(|b| over_budget(&b)),
            None => Ok(()),
        }
    }

    /// Count a chunk of a response body.
    fn received(&self, bytes: u64) -> Result<(), Error> {
        match &self.guard {
            Some(g) => g.add_bytes(bytes).map_err(|b| over_budget(&b)),
            None => Ok(()),
        }
    }

    /// Resolves when a request in flight is to be cut: as soon as the run is cancelled, or,
    /// for the creation of the upload of `created` (its path), [`CREATE_GRACE`] later.
    async fn stop(&self, created: Option<&str>) {
        self.cancel.cancelled().await;
        if created.is_some() {
            tokio::time::sleep(CREATE_GRACE).await;
        }
    }

    /// The error of a request cut by [`Gate::stop`]. The upload of `created` is reported: it
    /// may exist, and cannot be aborted.
    fn cut(&self, created: Option<&str>) -> Error {
        if let Some(path) = created {
            self.aborts.note_failure(
                path,
                format_args!(
                    "its creation got no answer within {}s of the stop, so there is no upload \
                     id to abort",
                    CREATE_GRACE.as_secs()
                ),
            );
        }
        cancelled()
    }
}

/// A transport behind a [`Gate`].
pub struct Gated<T> {
    inner: T,
    gate: Gate,
}

type BodyStream = Box<dyn Stream<Item = opendal::Result<Buffer>> + Send + Sync + Unpin>;
type Stop = Pin<Box<dyn Future<Output = ()> + Send + Sync>>;

impl<T: HttpTransport> HttpTransport for Gated<T> {
    async fn fetch(&self, req: http::Request<Buffer>) -> opendal::Result<http::Response<HttpBody>> {
        let (api, _) = Api::resolve(
            req.extensions().get::<ServiceOperation>().map(|s| s.0),
            req.method(),
            req.uri().path(),
            req.uri().query(),
        );
        if api == Api::AbortMultipart {
            return self.inner.fetch(req).await;
        }
        self.gate.admit(req.body().len() as u64)?;
        // The response of a created upload should arrive whole: see the module documentation.
        let created = (api == Api::CreateMultipart).then(|| req.uri().path().to_owned());
        let resp = tokio::select! {
            biased;
            r = self.inner.fetch(req) => r?,
            () = self.gate.stop(created.as_deref()) => {
                return Err(self.gate.cut(created.as_deref()));
            }
        };
        let gate = self.gate.clone();
        Ok(resp.map(|body| {
            body.map_inner(|inner| {
                let stop = {
                    let (gate, created) = (gate.clone(), created.clone());
                    Box::pin(async move { gate.stop(created.as_deref()).await }) as Stop
                };
                Box::new(GatedBody {
                    inner,
                    gate,
                    stop,
                    created,
                    done: false,
                }) as BodyStream
            })
        }))
    }
}

/// A response body behind the gate: its chunks are counted as they arrive, and it ends with
/// an error when `stop` resolves (the run is cancelled) or, unless it answers a created upload
/// (`created`: that body should arrive whole), at the chunk that exceeds the byte budget.
struct GatedBody {
    inner: BodyStream,
    gate: Gate,
    stop: Stop,
    created: Option<String>,
    done: bool,
}

impl Stream for GatedBody {
    type Item = opendal::Result<Buffer>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if this.done {
            return Poll::Ready(None);
        }
        if this.stop.as_mut().poll(cx).is_ready() {
            this.done = true;
            return Poll::Ready(Some(Err(this.gate.cut(this.created.as_deref()))));
        }
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                let counted = this.gate.received(chunk.len() as u64);
                match counted {
                    Err(e) if this.created.is_none() => {
                        this.done = true;
                        Poll::Ready(Some(Err(e)))
                    }
                    _ => Poll::Ready(Some(Ok(chunk))),
                }
            }
            Poll::Ready(None) => {
                this.done = true;
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::sync::Semaphore;

    use super::*;
    use crate::guard::Budgets;

    /// A transport that answers every request with `chunks` chunks of `chunk` bytes, once
    /// `answers` has a permit for it (`None`: at once).
    #[derive(Clone)]
    struct Fake {
        sent: Arc<AtomicU64>,
        answers: Option<Arc<Semaphore>>,
        chunks: usize,
        chunk: usize,
    }

    impl Fake {
        fn new(chunks: usize, chunk: usize) -> Self {
            Self {
                sent: Arc::default(),
                answers: None,
                chunks,
                chunk,
            }
        }

        fn sent(&self) -> u64 {
            self.sent.load(Ordering::SeqCst)
        }
    }

    impl HttpTransport for Fake {
        async fn fetch(
            &self,
            _req: http::Request<Buffer>,
        ) -> opendal::Result<http::Response<HttpBody>> {
            self.sent.fetch_add(1, Ordering::SeqCst);
            if let Some(answers) = &self.answers {
                answers.acquire().await.expect("open").forget();
            }
            let chunk = self.chunk;
            let body = futures::stream::iter(
                (0..self.chunks).map(move |_| Ok(Buffer::from(vec![0u8; chunk]))),
            );
            Ok(http::Response::new(HttpBody::new(body, None)))
        }
    }

    fn request(op: &'static str, body: usize) -> http::Request<Buffer> {
        http::Request::builder()
            .method(http::Method::PUT)
            .uri("http://lab/b/k")
            .extension(ServiceOperation(op))
            .body(Buffer::from(vec![0u8; body]))
            .unwrap()
    }

    fn gate(budgets: Budgets) -> (Gate, Arc<Guard>, Cancel) {
        let guard = Guard::new(budgets);
        let cancel = Cancel::new();
        guard.arm(cancel.clone());
        (
            Gate::new(Some(guard.clone()), cancel.clone(), Aborts::new()),
            guard,
            cancel,
        )
    }

    /// Send `req`: the error, or the response.
    async fn send<T: HttpTransport>(
        t: &T,
        req: http::Request<Buffer>,
    ) -> Result<http::Response<HttpBody>, String> {
        t.fetch(req).await.map_err(|e| {
            assert!(!e.is_temporary(), "no layer may retry it: {e}");
            e.message().to_owned()
        })
    }

    /// Read a body to its end as OpenDAL does: the bytes delivered, and the error that ended it.
    async fn drain(resp: http::Response<HttpBody>) -> (u64, Option<Error>) {
        use opendal::raw::oio::ReadStream;
        let mut body = resp.into_body();
        let mut got = 0;
        loop {
            match body.read().await {
                Ok(b) if b.is_empty() => return (got, None),
                Ok(b) => got += b.len() as u64,
                Err(e) => return (got, Some(e)),
            }
        }
    }

    /// `--max-requests N` sends N requests, retries included, and never an N+1st; the request
    /// that spends the budget cancels nothing, the refused one stops the run.
    #[tokio::test]
    async fn a_request_past_the_budget_is_never_sent() {
        let (gate, guard, cancel) = gate(Budgets {
            max_requests_count: Some(3),
            ..Default::default()
        });
        let fake = Fake::new(0, 0);
        let t = gate.wrap(fake.clone());
        for _ in 0..3 {
            assert!(send(&t, request("PutObject", 10)).await.is_ok());
        }
        assert!(guard.tripped() && guard.breach().is_none());
        assert!(
            !cancel.is_cancelled(),
            "a budget spent exactly stops nothing"
        );
        let e = send(&t, request("PutObject", 10)).await.err().unwrap();
        assert_eq!(e, "request budget spent: 3 requests (limit 3)");
        assert_eq!(fake.sent(), 3);
        assert_eq!(guard.usage(), (3, 30));
        assert!(cancel.is_cancelled());
        assert_eq!(guard.breach().unwrap().budget, "requests");
        // Cleanup is never refused (and never counted).
        assert!(send(&t, request("AbortMultipartUpload", 0)).await.is_ok());
        assert_eq!(fake.sent(), 4);
        assert_eq!(guard.usage(), (3, 30));
    }

    /// `--max-bytes`: a request body that does not fit is not sent, and a response stops at the
    /// chunk that crosses the budget.
    #[tokio::test]
    async fn bytes_are_counted_as_they_stream() {
        let bytes = |max| Budgets {
            max_bytes: Some(max),
            ..Default::default()
        };
        let (gate, guard, cancel) = self::gate(bytes(1000));
        let fake = Fake::new(0, 0);
        let t = gate.wrap(fake.clone());
        assert!(send(&t, request("UploadPart", 600)).await.is_ok());
        let e = send(&t, request("UploadPart", 600)).await.err().unwrap();
        // The budget is not spent: the request does not fit in what is left of it (9.2 said
        // "byte budget spent: 600 B transferred (limit 1000 B)").
        assert_eq!(
            e,
            "the next request (600 B) does not fit the byte budget: 600 B of 1000 B used"
        );
        assert_eq!(fake.sent(), 1, "the body never left");
        assert_eq!(guard.usage(), (1, 600));
        assert!(cancel.is_cancelled());
        assert_eq!(guard.breach().unwrap().message(), e);

        // A download of 10 × 300 B against 1000 B: cut at the fourth chunk.
        let (gate, guard, cancel) = self::gate(bytes(1000));
        let t = gate.wrap(Fake::new(10, 300));
        let resp = send(&t, request("GetObject", 0)).await.unwrap();
        let (got, err) = drain(resp).await;
        assert_eq!(got, 900, "the chunk past the budget is not delivered");
        assert!(err.is_some_and(|e| !e.is_temporary()));
        assert_eq!(guard.usage().1, 1200, "but it was transferred: counted");
        assert!(cancel.is_cancelled());
        assert_eq!(guard.breach().unwrap().budget, "bytes");
    }

    /// Once the byte budget is spent exactly, nothing more is sent: 9.2 admitted a request
    /// without a body (a GET), let the target answer it, and cut the response at its first
    /// chunk, over the budget.
    #[tokio::test]
    async fn a_spent_byte_budget_refuses_a_request_without_a_body() {
        let (gate, guard, cancel) = self::gate(Budgets {
            max_bytes: Some(1000),
            ..Default::default()
        });
        let fake = Fake::new(1, 300);
        let t = gate.wrap(fake.clone());
        // 400 B up and 300 B down, twice: the second upload spends the budget exactly.
        let resp = send(&t, request("UploadPart", 400)).await.unwrap();
        assert_eq!(drain(resp).await.0, 300);
        assert!(send(&t, request("UploadPart", 300)).await.is_ok());
        assert_eq!(guard.usage(), (2, 1000));
        assert!(guard.tripped() && guard.breach().is_none());
        assert!(!cancel.is_cancelled(), "spent exactly: nothing is stopped");
        let e = send(&t, request("GetObject", 0)).await.err().unwrap();
        assert_eq!(e, "byte budget spent: 1000 B transferred (limit 1000 B)");
        assert_eq!(fake.sent(), 2, "the request never left");
        assert_eq!(guard.usage(), (2, 1000), "nothing past the budget");
        assert!(cancel.is_cancelled());
        assert_eq!(guard.breach().unwrap().observed, 1000.0);
    }

    /// A cancellation cuts the requests in flight and refuses the next ones, but lets a
    /// multipart upload being created finish (its id is needed to abort it) and its abort
    /// through.
    #[tokio::test]
    async fn a_cancellation_cuts_what_is_in_flight() {
        let cancel = Cancel::new();
        let aborts = Aborts::new();
        let gate = Gate::new(None, cancel.clone(), aborts.clone());
        let mut fake = Fake::new(1, 8);
        // No request is answered until the test says so.
        let answers = Arc::new(Semaphore::new(0));
        fake.answers = Some(answers.clone());
        let t = Arc::new(gate.wrap(fake.clone()));
        let spawn = |op: &'static str| {
            let t = t.clone();
            tokio::spawn(async move { send(&*t, request(op, 8)).await.map(|_| ()) })
        };
        let part = spawn("UploadPart");
        let create = spawn("CreateMultipartUpload");
        while fake.sent() < 2 {
            tokio::task::yield_now().await;
        }
        cancel.cancel();
        // The part is cut although its response never comes.
        assert_eq!(part.await.unwrap().unwrap_err(), "the run was cancelled");
        // The created upload is still waiting for its answer, and gets it.
        assert!(!create.is_finished());
        answers.add_permits(1);
        create.await.unwrap().expect("the created upload answers");
        assert!(send(&*t, request("PutObject", 1)).await.is_err());
        assert_eq!(fake.sent(), 2, "nothing new is sent");
        answers.add_permits(1);
        assert!(send(&*t, request("AbortMultipartUpload", 0)).await.is_ok());
        assert_eq!(aborts.failure(), None);
    }

    /// K1-5: a created upload that never answers is not waited for beyond its grace period
    /// once the run is cancelled (9.2 waited for ever: the run hung before its cleanup), and
    /// is reported, since it cannot be aborted.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_create_is_given_up_after_its_grace_period() {
        let cancel = Cancel::new();
        let aborts = Aborts::new();
        let gate = Gate::new(None, cancel.clone(), aborts.clone());
        let mut fake = Fake::new(1, 8);
        fake.answers = Some(Arc::new(Semaphore::new(0)));
        let t = Arc::new(gate.wrap(fake.clone()));
        let create = {
            let t = t.clone();
            tokio::spawn(async move { send(&*t, request("CreateMultipartUpload", 0)).await })
        };
        while fake.sent() < 1 {
            tokio::task::yield_now().await;
        }
        // Not cancelled: it may take as long as it takes.
        tokio::time::sleep(CREATE_GRACE * 100).await;
        assert!(!create.is_finished());
        cancel.cancel();
        tokio::time::sleep(CREATE_GRACE - Duration::from_millis(1)).await;
        assert!(!create.is_finished(), "within its grace period");
        tokio::time::sleep(Duration::from_millis(2)).await;
        let e = create.await.unwrap().err().expect("given up");
        assert_eq!(e, "the run was cancelled");
        let failure = aborts.failure().expect("reported");
        assert!(
            failure.contains("`/b/k`") && failure.contains("no upload id to abort"),
            "{failure}"
        );

        // The same for the body of its answer: headers that arrived, an id that did not.
        struct Stalled;
        impl HttpTransport for Stalled {
            async fn fetch(
                &self,
                _req: http::Request<Buffer>,
            ) -> opendal::Result<http::Response<HttpBody>> {
                let body = futures::stream::pending::<opendal::Result<Buffer>>();
                Ok(http::Response::new(HttpBody::new(body, None)))
            }
        }
        let (cancel, aborts) = (Cancel::new(), Aborts::new());
        let t = Gate::new(None, cancel.clone(), aborts.clone()).wrap(Stalled);
        let resp = send(&t, request("CreateMultipartUpload", 0)).await.unwrap();
        let read = tokio::spawn(drain(resp));
        cancel.cancel();
        tokio::time::sleep(CREATE_GRACE - Duration::from_millis(1)).await;
        assert!(!read.is_finished(), "within its grace period");
        tokio::time::sleep(Duration::from_millis(2)).await;
        assert!(read.await.unwrap().1.is_some());
        assert!(aborts.failure().is_some());
    }

    /// A response body in flight ends when the run is cancelled, even while no chunk arrives.
    #[tokio::test]
    async fn a_cancellation_ends_a_stalled_body() {
        struct Stalled;
        impl HttpTransport for Stalled {
            async fn fetch(
                &self,
                _req: http::Request<Buffer>,
            ) -> opendal::Result<http::Response<HttpBody>> {
                let body = futures::stream::pending::<opendal::Result<Buffer>>();
                Ok(http::Response::new(HttpBody::new(body, None)))
            }
        }
        let cancel = Cancel::new();
        let t = Gate::new(None, cancel.clone(), Aborts::new()).wrap(Stalled);
        let resp = send(&t, request("GetObject", 0)).await.unwrap();
        let read = tokio::spawn(drain(resp));
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert!(!read.is_finished());
        cancel.cancel();
        let (got, err) = read.await.unwrap();
        assert_eq!(got, 0);
        assert!(err.is_some());
    }
}
