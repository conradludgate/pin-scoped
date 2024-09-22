use std::future::Future;

use crate::ScopeState;

pub trait AsyncFnOnceRef<S, R> {
    fn call(self, state: ScopeState<'_, S>) -> impl Send + Future<Output = R>;
}

trait ScopeFnOnce<Arg>: FnOnce(Arg) -> Self::Ret {
    type Ret;
}
impl<F: FnOnce(A) -> R, A, R> ScopeFnOnce<A> for F {
    type Ret = R;
}

impl<S: 'static, F, R: Send + 'static> AsyncFnOnceRef<S, R> for F
where
    F: 'static + for<'state> ScopeFnOnce<ScopeState<'state, S>>,
    for<'state> <F as ScopeFnOnce<ScopeState<'state, S>>>::Ret: Send + Future<Output = R>,
{
    fn call(self, state: ScopeState<'_, S>) -> impl Send + Future<Output = R> {
        (self)(state)
    }
}
