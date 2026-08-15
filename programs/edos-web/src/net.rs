//! Loading a page on a thread of its own, so the window stays awake while the
//! network does not.
//!
//! `edos_http` is blocking, and a page is not one fetch: it is the document,
//! then every stylesheet it links and every image it shows, each with its own
//! connection. Doing that between two frames left the window unable to redraw,
//! scroll or close for as long as the slowest server took, which is the one
//! failure a browser cannot hide.
//!
//! So a load runs on a thread and reports what it is doing as it goes. A whole
//! [`Document`] crosses back, which is what the `Arc`s in `doc.rs` are for.

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;

use crate::css::Viewport;
use crate::doc::{self, Context, Document};

/// What a load says while it is happening.
pub enum Update {
    /// A resource is on the wire: the document first, then each subresource it
    /// turns out to refer to. The window shows this, so a page stuck on one
    /// slow server names it rather than looking hung.
    Fetching(String),
    /// The page is parsed, styled, and the window's now.
    Loaded(Box<Page>),
    /// Nothing was built. The window keeps the page it had and says why.
    Failed(String),
}

/// A page that finished loading.
pub struct Page {
    pub document: Document,
    /// Where it came from, which after a redirect is not where it was asked
    /// for.
    pub address: String,
}

/// The loader thread and the window's end of its channel.
///
/// One load at a time: a second `start` abandons the first rather than racing
/// it. Abandoning is by ticket, not by stopping the thread, because a thread
/// blocked in a TLS handshake cannot be interrupted -- it finishes into a
/// channel nobody is listening to any more.
pub struct Loader {
    updates: Sender<(u64, Update)>,
    inbox: Receiver<(u64, Update)>,
    /// The load whose updates still count.
    ticket: u64,
    /// Every subresource this window has fetched, kept across pages: the pages
    /// of a site link the same stylesheets.
    cache: doc::Cache,
}

impl Loader {
    pub fn new() -> Loader {
        let (updates, inbox) = channel();
        Loader {
            updates,
            inbox,
            ticket: 0,
            cache: doc::Cache::default(),
        }
    }

    /// Begin loading `target`, abandoning whatever was in flight.
    pub fn start(&mut self, target: &str, viewport: Viewport, reader: bool) {
        self.ticket += 1;
        let ticket = self.ticket;
        let updates = self.updates.clone();
        let target = target.to_string();
        let context = Context {
            viewport,
            reader,
            cache: doc::Cache::clone(&self.cache),
        };
        thread::spawn(move || load(&updates, ticket, &target, &context));
    }

    /// Stop caring about the load in flight. What it eventually produces is
    /// dropped.
    pub fn stop(&mut self) {
        self.ticket += 1;
    }

    /// The next update from the load in flight, or `None` when there is
    /// nothing new. Updates from an abandoned load are dropped here.
    pub fn poll(&mut self) -> Option<Update> {
        loop {
            match self.inbox.try_recv() {
                Ok((ticket, update)) if ticket == self.ticket => return Some(update),
                Ok(_) => continue,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return None,
            }
        }
    }
}

/// Fetch, parse and style one page, reporting each fetch it makes.
fn load(updates: &Sender<(u64, Update)>, ticket: u64, target: &str, context: &Context) {
    let say = |update| {
        let _ = updates.send((ticket, update));
    };
    say(Update::Fetching(target.to_string()));
    let (html, base) = match crate::load(target) {
        Ok(loaded) => loaded,
        Err(message) => return say(Update::Failed(message)),
    };
    let address = base.to_string();
    let fetch = |url: &str| {
        say(Update::Fetching(url.to_string()));
        crate::fetch_subresource(url)
    };
    let document = doc::parse(&html, base, &fetch, context);
    say(Update::Loaded(Box::new(Page { document, address })));
}
