use crate::FutureService;
use tower::util::{Oneshot, ServiceExt};

/// Immediately and infalliby creates (usually) a Service.
pub trait NewService<T> {
    type Service;

    fn new_service(&self, target: T) -> Self::Service;
}

/// A Layer that modifies inner `MakeService`s to be exposd as a `NewService`.
#[derive(Clone, Copy, Debug, Default)]
pub struct FromMakeServiceLayer(());

/// Modifies inner `MakeService`s to be exposd as a `NewService`.
#[derive(Clone, Copy, Debug)]
pub struct FromMakeService<S> {
    make_service: S,
}

#[derive(Clone, Debug)]
pub struct NewCloneService<S>(S);

// === impl NewService ===

impl<F, T, S> NewService<T> for F
where
    F: Fn(T) -> S,
{
    type Service = S;

    fn new_service(&self, target: T) -> Self::Service {
        (self)(target)
    }
}

// === impl FromMakeService ===

impl<S> FromMakeService<S> {
    pub fn layer() -> impl super::layer::Layer<S, Service = Self> {
        super::layer::mk(|make_service| Self { make_service })
    }
}

impl<T, S> NewService<T> for FromMakeService<S>
where
    S: tower::Service<T> + Clone,
{
    type Service = FutureService<Oneshot<S, T>, S::Response>;

    fn new_service(&self, target: T) -> Self::Service {
        FutureService::new(self.make_service.clone().oneshot(target))
    }
}

// === impl NewCloneService ===

impl<S> From<S> for NewCloneService<S> {
    fn from(inner: S) -> Self {
        Self(inner)
    }
}

impl<T, S: Clone> NewService<T> for NewCloneService<S> {
    type Service = S;

    fn new_service(&self, _: T) -> Self::Service {
        self.0.clone()
    }
}
