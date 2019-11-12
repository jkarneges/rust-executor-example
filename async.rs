use std::mem;
use std::time;
use std::thread;
use std::sync::{Arc, Mutex, Condvar};
use std::future::Future;
use std::pin::Pin;
use std::task::{Poll, Context, Waker, RawWaker, RawWakerVTable};

type PollFn = dyn FnMut(&mut Context) -> Poll<()>;

struct WrappedFuture {
    poll_fn: Box<PollFn>,
}

impl WrappedFuture {
    pub fn new<F>(mut f: F) -> Self
    where
        F: Future<Output = ()> + 'static
    {
        let c = move |cx: &mut Context| {
            let p: Pin<&mut F> = unsafe { Pin::new_unchecked(&mut f) };
            match p.poll(cx) {
                Poll::Ready(_) => Poll::Ready(()),
                Poll::Pending => Poll::Pending,
            }
        };

        Self {
            poll_fn: Box::new(c),
        }
    }

    pub fn poll(&mut self, cx: &mut Context) -> Poll<()> {
        (self.poll_fn)(cx)
    }
}

type SharedFuture = Arc<Mutex<WrappedFuture>>;

struct ExecutorData {
    need_poll: Vec<SharedFuture>,
    sleeping: Vec<SharedFuture>,
}

struct Executor {
    data: Arc<(Mutex<ExecutorData>, Condvar)>,
}

impl Executor {
    pub fn new() -> Self {
        let data = ExecutorData {
            need_poll: Vec::new(),
            sleeping: Vec::new(),
        };

        Self {
            data: Arc::new((Mutex::new(data), Condvar::new())),
        }
    }

    pub fn spawn<F>(&self, f: F)
    where
        F: Future<Output = ()> + 'static
    {
        let (lock, _) = &*self.data;

        let mut data = lock.lock().unwrap();

        data.need_poll.push(Arc::new(Mutex::new(WrappedFuture::new(f))));
    }

    pub fn wake(data: &mut Arc<(Mutex<ExecutorData>, Condvar)>, wf: &SharedFuture) {
        let (lock, cond) = &**data;

        let mut data = lock.lock().unwrap();

        let mut pos = None;
        for (i, f) in data.sleeping.iter().enumerate() {
            if Arc::ptr_eq(f, wf) {
                pos = Some(i);
                break;
            }
        }
        if pos.is_none() {
            // unknown future
            return
        }

        let pos = pos.unwrap();

        let f = data.sleeping.remove(pos);
        data.need_poll.push(f);

        cond.notify_one();
    }

    pub fn exec(&self) {
        loop {
            let (lock, cond) = &*self.data;

            let mut data = lock.lock().unwrap();

            if data.need_poll.is_empty() {
                if data.sleeping.is_empty() {
                    // no tasks, we're done
                    break;
                }

                data = cond.wait(data).unwrap();
            }

            let need_poll = mem::replace(
                &mut data.need_poll,
                Vec::new()
            );

            mem::drop(data);

            let mut need_sleep = Vec::new();

            for f in need_poll {
                let w = MyWaker {
                    data: Arc::clone(&self.data),
                    f: Arc::new(Mutex::new(Some(Arc::clone(&f)))),
                }.into_task_waker();

                let mut cx = Context::from_waker(&w);

                let result = {
                    f.lock().unwrap().poll(&mut cx)
                };
                match result {
                    Poll::Ready(_) => {},
                    Poll::Pending => {
                        need_sleep.push(f);
                    },
                }
            }

            let mut data = lock.lock().unwrap();

            data.sleeping.append(&mut need_sleep);
        }
    }
}

static VTABLE: RawWakerVTable = RawWakerVTable::new(
    vt_clone,
    vt_wake,
    vt_wake_by_ref,
    vt_drop
);

#[derive(Clone)]
struct MyWaker {
    data: Arc<(Mutex<ExecutorData>, Condvar)>,
    f: Arc<Mutex<Option<SharedFuture>>>,
}

impl MyWaker {
    fn into_task_waker(self) -> Waker {
        let w = Box::new(self);
        let rw = RawWaker::new(Box::into_raw(w) as *mut (), &VTABLE);
        unsafe { Waker::from_raw(rw) }
    }

    fn wake(mut self) {
        self.wake_by_ref();
    }

    fn wake_by_ref(&mut self) {
        let f: &mut Option<SharedFuture> = &mut self.f.lock().unwrap();
        if f.is_some() {
            let f: SharedFuture = f.take().unwrap();
            Executor::wake(&mut self.data, &f);
        }
    }
}

unsafe fn vt_clone(data: *const ()) -> RawWaker {
    let w = (data as *const MyWaker).as_ref().unwrap();
    let new_w = Box::new(w.clone());

    RawWaker::new(Box::into_raw(new_w) as *mut (), &VTABLE)
}

unsafe fn vt_wake(data: *const ()) {
    let w = Box::from_raw(data as *mut MyWaker);
    w.wake();
}

unsafe fn vt_wake_by_ref(data: *const ()) {
    let w = (data as *mut MyWaker).as_mut().unwrap();
    w.wake_by_ref();
}

unsafe fn vt_drop(data: *const ()) {
    Box::from_raw(data as *mut MyWaker);
}

struct TimerFuture {
    duration: time::Duration,
    handle: Option<thread::JoinHandle<()>>,
}

impl TimerFuture {
    fn new(duration: time::Duration) -> Self {
        Self {
            duration,
            handle: None,
        }
    }
}

impl Future for TimerFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<()> {
        match &self.handle {
            None => {
                let duration = self.duration;
                let waker = cx.waker().clone();
                self.handle = Some(thread::spawn(move || {
                    thread::sleep(duration);
                    waker.wake();
                }));
                Poll::Pending
            },
            Some(_) => {
                let handle = self.handle.take().unwrap();
                handle.join().unwrap();
                Poll::Ready(())
            },
        }
    }
}

// convenience wrapper
fn sleep(duration: time::Duration) -> TimerFuture {
    TimerFuture::new(duration)
}

fn main() {
    let e = Executor::new();
    e.spawn(async {
        println!("a");
        sleep(time::Duration::from_millis(200)).await;
        println!("c");
    });
    e.spawn(async {
        sleep(time::Duration::from_millis(100)).await;
        println!("b");
        sleep(time::Duration::from_millis(200)).await;
        println!("d");
    });
    e.exec();
}
