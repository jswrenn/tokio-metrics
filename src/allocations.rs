use once_cell::sync::OnceCell;
use dashmap::DashMap;
use tracking_allocator::{
    AllocationGroupId,
    AllocationGroupToken,
    AllocationLayer,
    AllocationRegistry,
    AllocationTracker,
    Allocator,
};
use std::{
    alloc::System,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{sync_channel, SyncSender},
        Arc, Barrier as StdBarrier,
    },
    thread,
    time::Duration,
};

enum AllocationEvent {
    Allocated {
        addr: usize,
        size: usize,
        group_id: AllocationGroupId,
    },
    Deallocated {
        addr: usize,
    },
}

struct ChannelBackedTracker {
    // Our sender is using a bounded (fixed-size) channel to push allocation events to.  This is
    // important because we do _not_ want to allocate in order to actually send our allocation
    // events.  We would end up in a reentrant loop that could either overflow the stack, or
    // deadlock, depending on what the tracker does under the hood.
    //
    // This can happen for many different resources we might take for granted.  For example, we
    // can't write to stdout via `println!` as that would allocate, which causes an issue where the
    // reentrant call to `println!` ends up hitting a mutex around stdout, deadlocking the process.
    //
    // Also take care to note that the `AllocationEvent` structure has a fixed size and requires no
    // allocations itself to create.  This follows the same principle as using a bounded channel.
    sender: SyncSender<AllocationEvent>,
}

// This is our tracker implementation.  You will always need to create an implementation of
// `AllocationTracker` in order to actually handle allocation events.  The interface is
// straightforward: you're notified when an allocation occurs, and when a deallocation occurs.
impl tracking_allocator::AllocationTracker for ChannelBackedTracker {
    fn allocated(
        &self,
        addr: usize,
        size: usize,
        group_id: AllocationGroupId,
        _tags: Option<&'static [(&'static str, &'static str)]>,
    ) {
        // Allocations have all the pertinent information upfront, which you must store if you want
        // to do any correlation with deallocations.
        let f = self.sender.try_send(AllocationEvent::Allocated {
            addr,
            size,
            group_id,
        });
    }

    fn deallocated(&self, addr: usize) {
        // As `tracking_allocator` itself strives to add as little overhead as possible, we only
        // forward the address being deallocated.  Your tracker implementation will need to handle
        // mapping the allocation address back to allocation group if you need to know the total
        // in-use memory, vs simply knowing how many or when allocations are occurring.
        let _ = self.sender.try_send(AllocationEvent::Deallocated { addr });
    }
}

impl From<SyncSender<AllocationEvent>> for ChannelBackedTracker {
    fn from(sender: SyncSender<AllocationEvent>) -> Self {
        ChannelBackedTracker { sender }
    }
}

#[derive(Default)]
pub struct AllocationInfo {
    pub(crate) group_to_metrics: DashMap<AllocationGroupId, Arc<crate::task::Metrics>>,
    addr_to_group_size: DashMap<usize, (AllocationGroupId, usize)>,
}

pub(crate) static ALLOCATION_INFO : OnceCell<AllocationInfo> = OnceCell::new();

pub fn track_allocations() {

    let _ = ALLOCATION_INFO.set(AllocationInfo::default());

    fn create_arc_pair<T>(inner: T) -> (Arc<T>, Arc<T>) {
        let first = Arc::new(inner);
        let second = Arc::clone(&first);

        (first, second)
    }

    // Create our channels for receiving the allocation events.
    let (tx, rx) = sync_channel(10000);
    let (done, should_exit) = create_arc_pair(AtomicBool::new(false));
    let (is_flushed, mark_flushed) = create_arc_pair(StdBarrier::new(2));

    // We spawn off our processing thread so the channels don't back up as we're exeucting.
    let _ = thread::spawn(move || {
        let _ = AllocationRegistry::set_global_tracker(ChannelBackedTracker::from(tx))
          .expect("no other global tracker should be set yet");
      
        loop {
            println!(".");
            // We're only using a timeout here so that we ensure that we're checking to see if we
            // should actually finish up and exit.
            if let Ok(event) = rx.recv_timeout(Duration::from_millis(10)) {
                use core::sync::atomic::Ordering::SeqCst;
                match event {
                    AllocationEvent::Allocated {addr, size, group_id} =>
                        if let Some(info) = ALLOCATION_INFO.get() {
                            println!("x");
                            info.addr_to_group_size.insert(addr, (group_id.clone(), size));
                            println!("y");
                            {if let Some(metrics) = info.group_to_metrics.get(&group_id) {
                                println!("z");
                                metrics.bytes_allocated.fetch_add(size as u64, SeqCst);
                            } else {
                                println!("z");
                            }}
                        },
                    AllocationEvent::Deallocated { addr } => 
                        if let Some(info) = ALLOCATION_INFO.get() {
                            if let Some((_, (group_id, size))) = info.addr_to_group_size.remove(&addr) {
                                if let Some(metrics) = info.group_to_metrics.get(&group_id) {
                                    metrics.bytes_freed.fetch_sub(size as u64, SeqCst);
                                }
                            }
                        },
                }
            } else {
                println!("x");
            }

            // NOTE: Since the global tracker holds the sender side of the channel, if we just did
            // blocking receives until we got `None` back, then we would hang... because the sender
            // won't actually ever drop.
            //
            // If we don't need 100% accurate reporting, your worker thread/task could likely just
            // skip doing any sort of synchronization, and use blocking receives, since the process
            // exit would kill the thread no matter what.
            //
            // Otherwise, you need some sort of "try receiving with a timeout, check the shutdown
            // flag, then try to receive again" loop, like we have here.
            if should_exit.load(Ordering::Relaxed) {
                break;
            }
        }

        // Let the main thread know that we're done.
        let _ = mark_flushed.wait();
    });
    
    AllocationRegistry::enable_tracking();
}
