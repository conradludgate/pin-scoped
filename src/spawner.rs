use std::{
    future::{poll_fn, Future},
    pin::{pin, Pin},
    sync::Mutex,
    task::Poll,
};

use diatomic_waker::DiatomicWaker;
use tokio::sync::Semaphore;

use crate::Scope;

pub struct StateWithSpawner<State, Task> {
    pub state: State,
    spawner: SpawnerChannel<Task>,
}

impl<State, Task> StateWithSpawner<State, Task> {
    pub const fn new(state: State) -> Self {
        Self {
            state,
            spawner: SpawnerChannel::new(),
        }
    }

    pub async fn spawn(&self, t: Task) {
        self.spawner.spawn(t).await
    }
}

impl<State: 'static, Task> Scope<StateWithSpawner<State, Task>> {
    pub async fn pop_task(self: Pin<&mut Self>) -> Option<Task> {
        let state = self.as_ref().get();

        // SAFETY: we have &mut access to the scope. Cannot be running concurrently
        let mut pop = pin!(unsafe { state.spawner.pop() });

        poll_fn(|cx| {
            if self.as_ref().poll_until_empty(cx).is_ready() {
                Poll::Ready(None)
            } else {
                pop.as_mut().poll(cx).map(Some)
            }
        })
        .await
    }
}

pub struct SpawnerChannel<Task> {
    spawn_queue: Semaphore,
    next_task: Mutex<Option<Task>>,
    waker: DiatomicWaker,
}

impl<Task: Default> Default for SpawnerChannel<Task> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Task> SpawnerChannel<Task> {
    pub const fn new() -> Self {
        Self {
            spawn_queue: Semaphore::const_new(1),
            next_task: Mutex::new(None),
            waker: DiatomicWaker::new(),
        }
    }

    pub async fn spawn(&self, t: Task) {
        self.spawn_queue.acquire().await.unwrap().forget();
        *self.next_task.lock().unwrap() = Some(t);
        self.waker.notify();
    }

    /// Get a task out of the channel.
    ///
    /// # Safety
    ///
    /// Since this channel is an MPSC, pop() must only be called from one thread at a time.
    pub async unsafe fn pop(&self) -> Task {
        let get_task = || self.next_task.lock().unwrap().take();
        let task = unsafe { self.waker.wait_until(get_task).await };
        self.spawn_queue.add_permits(1);
        task
    }
}
