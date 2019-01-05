//! oppgave - A simple Redis-based task queue
//!
//! oppgave provides a small reliable queue on top of Redis.
//! It allows to push tasks and fetch them again.
//!
//! Tasks can be arbitrary objects, as long as they can be encoded and decoded into a String.
//! The easiest way is to rely on JSON encoding by marking the task as `Serialize` and
//! `Deserialize`.
//!
//! Oppgave prodives a [reliable queue](http://redis.io/commands/rpoplpush#pattern-reliable-queue)
//! by moving acquired tasks to a backup queue.
//! If a task finished it is removed from this backup queue.
//! If a task fails it remains in the backup queue for human processing later on.
//!
//! See [`Queue`](struct.Queue.html) for a detailed documentation how to use this.
//!
//! The following examples are provided as executables as well:
//!
//! ## Example: Producer
//!
//! ```rust,ignore
//! #[derive(Deserialize, Serialize)]
//! struct Job { id: u64 }
//!
//! let client = redis::Client::open("redis://127.0.0.1/").unwrap();
//! let con = client.get_connection().unwrap();
//! let producer = Queue::new("default".into(), con);
//!
//! producer.push(Job{ id: 42 });
//! ```
//!
//! ## Example: Worker
//!
//! ```rust,ignore
//! #[derive(Deserialize, Serialize)]
//! struct Job { id: u64 }
//!
//! let client = redis::Client::open("redis://127.0.0.1/").unwrap();
//! let con = client.get_connection().unwrap();
//! let worker = Queue::new("default".into(), con);
//!
//! while let Some(task) = worker.next() {
//!     println!("Working with Job {}", job.id);
//! }
//! ```

#![deny(missing_docs)]

#[cfg(test)]
#[macro_use]
extern crate serde_derive;

extern crate serde;
extern crate serde_json;
extern crate redis;
extern crate libc;

use std::{str, thread};
use std::cell::Cell;
use std::ops::{Deref, Drop};
use std::convert::From;
use serde::de::DeserializeOwned;
use serde::ser::Serialize;
use redis::{Value, RedisResult, ErrorKind, Commands};

/// Return the PID of the calling process.
/// TODO: Does this work on Windows?
fn getpid() -> i32 {
    unsafe { libc::getpid() as i32 }
}

/// Task objects that can be reconstructed from the data stored in Redis
///
/// Implemented for all `Deserialize` objects by default by relying on JSON encoding.
pub trait TaskDecodable
where
    Self: Sized,
{
    /// Decode the given Redis value into a task
    ///
    /// This should decode the string value into a proper task.
    /// The string value is encoded as JSON.
    fn decode_task(value: &Value) -> RedisResult<Self>;
}

/// Task objects that can be encoded to a string to be stored in Redis
///
/// Implemented for all `Serialize` objects by default by encoding as JSON.
pub trait TaskEncodable {
    /// Encode the value into a Blob to insert into Redis
    ///
    /// It should encode the value into a string.
    fn encode_task(&self) -> Vec<u8>;
}

impl<T: DeserializeOwned> TaskDecodable for T {
    fn decode_task(value: &Value) -> RedisResult<T> {
        match *value {
            Value::Data(ref v) => {
                serde_json::from_slice(v).map_err(|_| {
                    From::from((ErrorKind::TypeError, "JSON decode failed"))
                })
            }
            _ => try!(Err((ErrorKind::TypeError, "Can only decode from a string"))),
        }
    }
}

impl<T: Serialize> TaskEncodable for T {
    fn encode_task(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap()
    }
}

/// A wrapper of the fetched task.
///
/// If not marked otherwise, the contained task will be removed from the backup queue on `Drop`.
/// Call `fail()` to mark the processing as failed. The task will remain in the backup queue.
///
/// It derefs to the underlying task automatically for all other method calls.
pub struct TaskGuard<'a, T: 'a> {
    task: T,
    queue: &'a Queue,
    failed: Cell<bool>,
}

impl<'a, T> TaskGuard<'a, T> {
    /// Fail the current task, in order to keep it in the backup queue.
    pub fn fail(&self) {
        self.failed.set(true);
    }

    /// Get access to the underlying task.
    ///
    /// This should only be needed in very few cases, as this guard derefs automatically.
    pub fn inner(&self) -> &T {
        &self.task
    }

    /// Get access to the wrapper queue.
    pub fn queue(&self) -> &Queue {
        self.queue
    }
}

impl<'a, T> Deref for TaskGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.task
    }
}

impl<'a, T> Drop for TaskGuard<'a, T> {
    fn drop(&mut self) {
        if !self.failed.get() {
            // Pop job from backup queue
            let backup = &self.queue.backup_queue[..];
            self.queue.client.lpop::<_, ()>(backup).expect(
                "LPOP from backup queue failed",
            );
        }
    }
}

/// A Queue allows to push new tasks or fetch and decode them for processing.
///
/// ## Push
///
/// Pushing new tasks to the queue encodes the given object and stores it in Redis for later
/// processing.
///
/// ## Example
///
/// ```rust,ignore
/// #[derive(Deserialize, Serialize)]
/// struct Job { id: u64 }
///
/// let client = redis::Client::open("redis://127.0.0.1/").unwrap();
/// let con = client.get_connection().unwrap();
/// let producer = Queue::new("default".into(), con);
///
/// producer.push(Job{ id: 42 });
/// ```
///
///
/// ## Fetch
///
/// A Queue provides a convenient `Iterator`-like interface over tasks:
///
/// ```rust,ignore
/// #[derive(Deserialize, Serialize)]
/// struct Job { id: u64 }
///
/// let client = redis::Client::open("redis://127.0.0.1/").unwrap();
/// let con = client.get_connection().unwrap();
/// let queue = Queue::new("default".into(), con);
///
/// while let Some(task) = queue.next() {
///     println!("Working with Job {}", job.id);
/// }
/// ```
///
/// Fetching a task from a queue returns a wrapper object, which delegates to the underlying,
/// automatically decoded task object.
/// If this wrapper object is dropped, the task is considered complete and therefore removed from
/// the queue.
/// If the task processing fails, you need to call `fail()` on this wrapper.
///
/// ### Example: Task complete & task failed
///
/// ```rust,ignore
/// struct Job { id: u64 }
///
/// let client = redis::Client::open("redis://127.0.0.1/").unwrap();
/// let con = client.get_connection().unwrap();
/// let worker = Queue::new("default".into(), con);
///
/// {
///   // `next` gives an Option<Result<...>>
///   let task = worker.next().unwrap().unwrap();
/// } // Task succeeded, removed from backup queue
///
/// {
///   let task = worker.next().unwrap().unwrap();
///   task.fail();
/// } // Task failed, stays in backup queue
/// ```
///
///
#[derive(Clone)]
pub struct Queue {
    queue_name: String,
    backup_queue: String,
    stopped: Cell<bool>,
    client: redis::Client,
}

impl Queue {
    /// Create a new Queue for the given name
    pub fn new(name: String, client: redis::Client) -> Queue {
        let qname = format!("oppgave:{}", name);
        let backup_queue = format!(
            "{}:{}:{}",
            qname,
            getpid(),
            thread::current().name().unwrap_or("default".into())
        );

        Queue {
            queue_name: qname,
            backup_queue: backup_queue,
            client: client,
            stopped: Cell::new(false),
        }
    }

    fn connection(&self) -> RedisResult<redis::Connection> {
        self.client.get_connection()
    }

    /// Stop processing the queue
    ///
    /// On the next `.next()` call `None` will be returned.
    pub fn stop(&self) {
        self.stopped.set(true);
    }

    /// Check if queue processing is stopped
    pub fn is_stopped(&self) -> bool {
        self.stopped.get()
    }

    /// Get the full queue name
    pub fn queue(&self) -> &str {
        &self.queue_name
    }

    /// Get the full backup queue name
    pub fn backup_queue(&self) -> &str {
        &self.backup_queue
    }

    /// Get the number of remaining tasks in the queue
    pub fn size(&self) -> u64 {
        self.connection().and_then(|con| con.llen(self.queue())).unwrap_or(0)
    }

    /// Push a new task to the queue
    pub fn push<T: TaskEncodable>(&self, task: T) -> RedisResult<()> {
        self.connection()?.lpush(self.queue(), task.encode_task())
    }

    /// Grab the next task from the queue
    ///
    /// This method blocks and waits until a new task is available.
    pub fn next<T: TaskDecodable>(&self) -> Option<RedisResult<TaskGuard<T>>> {
        if self.stopped.get() {
            return None;
        }

        let v;
        {
            let qname = &self.queue_name[..];
            let backup = &self.backup_queue[..];

            v = match self.connection().and_then(|con| con.brpoplpush(qname, backup, 0)) {
                Ok(v) => v,
                Err(_) => {
                    return Some(Err(From::from((ErrorKind::TypeError, "next failed"))));
                }
            };
        }

        let v = match v {
            v @ Value::Data(_) => v,
            _ => {
                return Some(Err(
                    From::from((ErrorKind::TypeError, "Not a proper reply")),
                ));
            }
        };

        match T::decode_task(&v) {
            Err(e) => Some(Err(e)),
            Ok(task) => Some(Ok(TaskGuard {
                task: task,
                queue: self,
                failed: Cell::new(false),
            })),
        }
    }
}


#[cfg(test)]
mod test {
    extern crate redis;

    use redis::Commands;
    use super::{Queue, TaskGuard};

    #[derive(Deserialize, Serialize)]
    struct Job {
        id: u64,
    }

    #[test]
    fn decodes_job() {
        let client = redis::Client::open("redis://127.0.0.1:6379/").unwrap();
        let con = client.get_connection().unwrap();
        let worker = Queue::new("default".into(), client);

        let _: () = con.rpush(worker.queue(), "{\"id\":42}").unwrap();

        let j = worker.next::<Job>().unwrap().unwrap();
        assert_eq!(42, j.id);
    }

    #[test]
    fn releases_job() {
        let client = redis::Client::open("redis://127.0.0.1:6379/").unwrap();
        let con = client.get_connection().unwrap();
        let worker = Queue::new("default".into(), client);
        let bqueue = worker.backup_queue();

        let _: () = con.del(bqueue).unwrap();
        let _: () = con.lpush(worker.queue(), "{\"id\":42}").unwrap();

        {
            let j = worker.next::<Job>().unwrap().unwrap();
            assert_eq!(42, j.id);
            let in_backup: Vec<String> = con.lrange(bqueue, 0, -1).unwrap();
            assert_eq!(1, in_backup.len());
            assert_eq!("{\"id\":42}", in_backup[0]);
        }

        let in_backup: u32 = con.llen(bqueue).unwrap();
        assert_eq!(0, in_backup);
    }

    #[test]
    fn can_be_stopped() {
        let client = redis::Client::open("redis://127.0.0.1:6379/").unwrap();
        let con = client.get_connection().unwrap();
        let worker = Queue::new("stopper".into(), client);

        let _: () = con.del(worker.queue()).unwrap();
        let _: () = con.lpush(worker.queue(), "{\"id\":1}").unwrap();
        let _: () = con.lpush(worker.queue(), "{\"id\":2}").unwrap();
        let _: () = con.lpush(worker.queue(), "{\"id\":3}").unwrap();

        assert_eq!(3, worker.size());

        while let Some(task) = worker.next::<Job>() {
            let _task = task.unwrap();
            worker.stop();
        }

        assert_eq!(2, worker.size());
    }

    #[test]
    fn can_enqueue() {
        let client = redis::Client::open("redis://127.0.0.1:6379/").unwrap();
        let con = client.get_connection().unwrap();

        let worker = Queue::new("enqueue".into(), client);
        let _: () = con.del(worker.queue()).unwrap();

        assert_eq!(0, worker.size());

        worker.push(Job { id: 53 }).unwrap();

        assert_eq!(1, worker.size());

        let j = worker.next::<Job>().unwrap().unwrap();
        assert_eq!(53, j.id);
    }

    #[test]
    fn does_not_drop_failed() {
        let client = redis::Client::open("redis://127.0.0.1:6379/").unwrap();
        let con = client.get_connection().unwrap();
        let worker = Queue::new("failure".into(), client);

        let _: () = con.del(worker.queue()).unwrap();
        let _: () = con.del(worker.backup_queue()).unwrap();
        let _: () = con.lpush(worker.queue(), "{\"id\":1}").unwrap();

        {
            let task: TaskGuard<Job> = worker.next().unwrap().unwrap();
            task.fail();
        }

        let len: u32 = con.llen(worker.backup_queue()).unwrap();
        assert_eq!(1, len);
    }
}
