use crate::{Error, Result};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

struct StopOnPanic<'a>(&'a AtomicBool);
impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.store(true, Ordering::Relaxed);
        }
    }
}

pub(crate) fn map<T: Send, U: Send>(
    items: Vec<T>,
    work: impl Fn(T) -> Result<U> + Sync,
) -> Result<Vec<U>> {
    if items.len() < 16 {
        return items.into_iter().map(work).collect();
    }
    map_bounded(items, 8, work)
}

pub(crate) fn map_bounded<T: Send, U: Send>(
    items: Vec<T>,
    limit: usize,
    work: impl Fn(T) -> Result<U> + Sync,
) -> Result<Vec<U>> {
    let len = items.len();
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(limit.max(1))
        .min(len.max(1));
    if len < 2 || workers == 1 {
        return items.into_iter().map(work).collect();
    }
    let queue = Mutex::new(items.into_iter().enumerate());
    let stopped = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let mut handles = vec![];
        for _ in 0..workers {
            let (queue, stopped, work) = (&queue, &stopped, &work);
            handles.push(scope.spawn(move || -> Result<Vec<(usize, U)>> {
                let _guard = StopOnPanic(stopped);
                let mut results = vec![];
                while !stopped.load(Ordering::Relaxed) {
                    let next = queue
                        .lock()
                        .map_err(|_| {
                            stopped.store(true, Ordering::Relaxed);
                            Error::new("E_STATE", "filesystem worker queue poisoned")
                        })?
                        .next();
                    let Some((index, item)) = next else {
                        break;
                    };
                    match work(item) {
                        Ok(value) => results.push((index, value)),
                        Err(e) => {
                            stopped.store(true, Ordering::Relaxed);
                            return Err(e);
                        }
                    }
                }
                Ok(results)
            }));
        }
        let mut results = Vec::with_capacity(len);
        let mut error = None;
        // Every worker must stop before returning an error: another worker can
        // still hold files belonging to the operation's recovery journals.
        for handle in handles {
            match handle.join() {
                Ok(Ok(values)) => results.extend(values),
                Ok(Err(e)) => {
                    if error.is_none() {
                        error = Some(e);
                    }
                }
                Err(_) => {
                    if error.is_none() {
                        error = Some(Error::new("E_STATE", "filesystem worker panicked"));
                    }
                }
            }
        }
        if let Some(e) = error {
            return Err(e);
        }
        if results.len() != len {
            return Err(Error::new(
                "E_STATE",
                "filesystem workers returned incomplete results",
            ));
        }
        results.sort_unstable_by_key(|(index, _)| *index);
        Ok(results.into_iter().map(|(_, value)| value).collect())
    })
}
