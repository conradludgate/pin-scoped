use std::future::Future;

use crate::{Runtime, ScopeState};

pub trait AsyncFnOnceRef<S, R, Rt: Runtime> {
    fn call(self, state: ScopeState<'_, S, Rt>) -> impl Send + Future<Output = R>;
}

trait ScopeFnOnce<Arg>: FnOnce(Arg) -> Self::Ret {
    type Ret;
}
impl<F: FnOnce(A) -> R, A, R> ScopeFnOnce<A> for F {
    type Ret = R;
}

impl<S: 'static, F, R: Send + 'static, Rt: Runtime> AsyncFnOnceRef<S, R, Rt> for F
where
    F: 'static + for<'state> ScopeFnOnce<ScopeState<'state, S, Rt>>,
    for<'state> <F as ScopeFnOnce<ScopeState<'state, S, Rt>>>::Ret: Send + Future<Output = R>,
{
    fn call(self, state: ScopeState<'_, S, Rt>) -> impl Send + Future<Output = R> {
        (self)(state)
    }
}
