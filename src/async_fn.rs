use std::{future::Future, pin::Pin};

pub trait AsyncFnOnceRef<S, R> {
    fn call(self, state: Pin<&S>) -> impl Send + Future<Output = R>;
}

trait PinFnOnce<Arg>: FnOnce(Pin<Arg>) -> Self::Ret {
    type Ret;
}
impl<F: FnOnce(Pin<A>) -> R, A, R> PinFnOnce<A> for F {
    type Ret = R;
}

impl<S: 'static, F, R: Send + 'static> AsyncFnOnceRef<S, R> for F
where
    F: 'static + for<'state> PinFnOnce<&'state S>,
    for<'state> <F as PinFnOnce<&'state S>>::Ret: Send + Future<Output = R>,
{
    fn call(self, state: Pin<&S>) -> impl Send + Future<Output = R> {
        (self)(state)
    }
}
