//! Actix tracing - support for tokio tracing with Actix services.
// #![deny(rust_2018_idioms, warnings)]
#![deny(rust_2018_idioms)]

use std::cell::RefCell;
use std::future::Future;
use std::marker::PhantomData;
use std::rc::Rc;
use std::task::{Context, Poll};

use actix_service::{
    apply, dev::ApplyTransform, IntoServiceFactory, Service, ServiceFactory, Transform,
};
use futures_util::future::{ok, Either, FutureExt, Map, Ready};
use tracing_futures::{Instrument, Instrumented};

struct MapConfigState<S, R> {
    service: S,
    data: Option<R>,
}

pub struct MapConfigService<S, R> {
    inner: Rc<RefCell<MapConfigState<S, R>>>,
}

impl<S, R> MapConfigService<S, R> {
    fn new(service: S, data: Option<R>) -> Self {
        MapConfigService {
            inner: Rc::new(RefCell::new(MapConfigState { service, data })),
        }
    }
}

impl<S, R> Clone for MapConfigService<S, R> {
    fn clone(&self) -> Self {
        MapConfigService {
            inner: self.inner.clone(),
        }
    }
}

impl<S, R> Service for MapConfigService<S, R>
where
    S: Service,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.borrow_mut().service.poll_ready(ctx)
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        self.inner.borrow_mut().service.call(req)
    }
}

struct MapConfigFactoryState<S, M, R> {
    service_factory: S,
    map_cfg: M,
    _data: PhantomData<R>,
}

pub struct MapConfigFactory<S, M, R> {
    inner: Rc<RefCell<MapConfigFactoryState<S, M, R>>>,
}

impl<S, M, R> MapConfigFactory<S, M, R> {
    fn new(service_factory: S, map_cfg: M) -> Self {
        MapConfigFactory {
            inner: Rc::new(RefCell::new(MapConfigFactoryState {
                service_factory,
                map_cfg,
                _data: PhantomData,
            })),
        }
    }
}

impl<S, M, R> Clone for MapConfigFactory<S, M, R> {
    fn clone(&self) -> Self {
        MapConfigFactory {
            inner: self.inner.clone(),
        }
    }
}

impl<S, M, R> ServiceFactory for MapConfigFactory<S, M, R>
where
    S: ServiceFactory,
    M: Fn(&S::Config) -> Option<R>,
    R: 'static,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Config = S::Config;
    type Service = MapConfigService<S::Service, R>;
    type InitError = S::InitError;
    type Future = Map<
        S::Future,
        Box<
            dyn FnOnce(
                <S::Future as Future>::Output,
            ) -> Result<MapConfigService<S::Service, R>, S::InitError>,
        >,
    >;

    fn new_service(&self, cfg: Self::Config) -> Self::Future {
        // map the config and store the transformed data
        let data = (self.inner.borrow().map_cfg)(&cfg);

        // TODO: Is there a way we can avoid the Box::new below?
        self.inner
            .borrow()
            .service_factory
            .new_service(cfg)
            .map(Box::new(move |res| {
                res.map(|service| MapConfigService::new(service, data))
            }))
    }
}

/// A `Service` implementation that automatically enters/exits tracing spans
/// for the wrapped inner service.
#[derive(Clone)]
pub struct TracingService<S, R, F> {
    inner: MapConfigService<S, R>,
    make_span: F,
}

impl<S, R, F> TracingService<S, R, F> {
    pub fn new(inner: MapConfigService<S, R>, make_span: F) -> Self {
        TracingService { inner, make_span }
    }
}

impl<S, R, F> Service for TracingService<S, R, F>
where
    S: Service,
    F: Fn(Option<&R>, &S::Request) -> Option<tracing::Span>,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = Either<S::Future, Instrumented<S::Future>>;

    fn poll_ready(&mut self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(ctx)
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        let span = (self.make_span)(self.inner.inner.borrow().data.as_ref(), &req);
        let _enter = span.as_ref().map(|s| s.enter());

        let fut = self.inner.call(req);

        // make a child span to track the future's execution
        if let Some(span) = span
            .clone()
            .map(|span| tracing::span!(parent: &span, tracing::Level::INFO, "future"))
        {
            Either::Right(fut.instrument(span))
        } else {
            Either::Left(fut)
        }
    }
}

/// A `Transform` implementation that wraps services with a [`TracingService`].
///
/// [`TracingService`]: struct.TracingService.html
pub struct TracingTransform<S, U, F, R> {
    make_span: F,
    _p: PhantomData<fn(S, U, R)>,
}

impl<S, U, F, R> TracingTransform<S, U, F, R> {
    pub fn new(make_span: F) -> Self {
        TracingTransform {
            make_span,
            _p: PhantomData,
        }
    }
}

impl<S, U, F, R> Transform<MapConfigService<S, R>> for TracingTransform<S, U, F, R>
where
    S: Service,
    U: ServiceFactory<
        Request = S::Request,
        Response = S::Response,
        Error = S::Error,
        Service = S,
    >,
    F: Fn(Option<&R>, &S::Request) -> Option<tracing::Span> + Clone,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Transform = TracingService<S, R, F>;
    type InitError = U::InitError;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: MapConfigService<S, R>) -> Self::Future {
        ok(TracingService::new(service, self.make_span.clone()))
    }
}

/// Wraps the provided service factory with a transform that automatically
/// enters/exits the given span.
///
/// The span to be entered/exited can be provided via a closure. The closure
/// is passed in a reference to the request being handled by the service.
///
/// For example:
/// ```rust,ignore
/// let traced_service = trace(
///     web_service,
///     |req: &Request| Some(span!(Level::INFO, "request", req.id))
/// );
/// ```
pub fn trace<S, U, M, R, F>(
    service_factory: U,
    map_cfg: M,
    make_span: F,
) -> ApplyTransform<
    TracingTransform<MapConfigService<S::Service, R>, S, F, R>,
    MapConfigService<S::Service, R>,
>
where
    S: ServiceFactory,
    U: IntoServiceFactory<S>,
    M: Fn(&S::Config) -> Option<R>,
    F: Fn(Option<&R>, &S::Request) -> Option<tracing::Span> + Clone,
{
    let service_factory = MapConfigFactory::new(service_factory.into_factory(), map_cfg);

    apply(TracingTransform::new(make_span), service_factory)
}

#[cfg(test)]
mod test {
    use super::*;

    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, RwLock};

    use actix_service::{fn_factory, fn_service};
    use slab::Slab;
    use tracing::{span, Event, Level, Metadata, Subscriber};

    thread_local! {
        static SPAN: RefCell<Vec<span::Id>> = RefCell::new(Vec::new());
    }

    #[derive(Default)]
    struct Stats {
        entered_spans: BTreeSet<u64>,
        exited_spans: BTreeSet<u64>,
        events_count: BTreeMap<u64, usize>,
    }

    #[derive(Default)]
    struct Inner {
        spans: Slab<&'static Metadata<'static>>,
        stats: Stats,
    }

    #[derive(Clone, Default)]
    struct TestSubscriber {
        inner: Arc<RwLock<Inner>>,
    }

    impl Subscriber for TestSubscriber {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, span: &span::Attributes<'_>) -> span::Id {
            let id = self.inner.write().unwrap().spans.insert(span.metadata());
            span::Id::from_u64(id as u64 + 1)
        }

        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

        fn event(&self, event: &Event<'_>) {
            let id = event
                .parent()
                .cloned()
                .or_else(|| SPAN.with(|current_span| current_span.borrow().last().cloned()))
                .unwrap();

            *self
                .inner
                .write()
                .unwrap()
                .stats
                .events_count
                .entry(id.into_u64())
                .or_insert(0) += 1;
        }

        fn enter(&self, span: &span::Id) {
            self.inner
                .write()
                .unwrap()
                .stats
                .entered_spans
                .insert(span.into_u64());

            SPAN.with(|current_span| {
                current_span.borrow_mut().push(span.clone());
            });
        }

        fn exit(&self, span: &span::Id) {
            self.inner
                .write()
                .unwrap()
                .stats
                .exited_spans
                .insert(span.into_u64());

            // we are guaranteed that on any given thread, spans are exited in reverse order
            SPAN.with(|current_span| {
                let leaving = current_span
                    .borrow_mut()
                    .pop()
                    .expect("told to exit span when not in span");
                assert_eq!(
                    &leaving, span,
                    "told to exit span that was not most recently entered"
                );
            });
        }
    }

    #[actix_rt::test]
    async fn service_call() {
        let service_factory = fn_factory(|| {
            ok::<_, ()>(fn_service(|req: &'static str| {
                tracing::event!(Level::TRACE, "It's happening - {}!", req);
                ok::<_, ()>(())
            }))
        });

        let subscriber = TestSubscriber::default();
        let _guard = tracing::subscriber::set_default(subscriber.clone());

        let span_svc = span!(Level::TRACE, "span_svc");
        let trace_service_factory = trace(service_factory, |_: &&str| Some(span_svc.clone()));
        let mut service = trace_service_factory.new_service(()).await.unwrap();
        service.call("boo").await.unwrap();

        let id = span_svc.id().unwrap().into_u64();
        assert!(subscriber
            .inner
            .read()
            .unwrap()
            .stats
            .entered_spans
            .contains(&id));
        assert!(subscriber
            .inner
            .read()
            .unwrap()
            .stats
            .exited_spans
            .contains(&id));
        assert_eq!(subscriber.inner.read().unwrap().stats.events_count[&id], 1);
    }
}
