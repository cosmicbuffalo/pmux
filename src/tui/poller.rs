//! Background readers for the sidebar's outer-tmux observations.
//!
//! Querying outer focus and enriching a state snapshot with outer locations
//! both run tmux commands (and, for locations, a process-tree walk and
//! per-process file reads). Done on the UI thread, a slow or hung tmux froze
//! the sidebar. They now run on two worker threads — one per kind, so a slow
//! location refresh never delays the focus highlight — and the UI thread only
//! applies their results.
//!
//! Each worker has at most one request in flight. A request made meanwhile
//! replaces any earlier waiting one, so a stuck server costs one late result
//! rather than a growing backlog, and a burst of snapshots is enriched once,
//! for the newest.

use super::*;

/// A worker thread taking requests one at a time; see the module docs.
pub(super) struct Lane<Request, Response> {
    requests: Sender<Request>,
    responses: Receiver<Result<Response, String>>,
    in_flight: bool,
    waiting: Option<Request>,
}

impl<Request: Send + 'static, Response: Send + 'static> Lane<Request, Response> {
    pub(super) fn spawn(
        name: &str,
        mut work: impl FnMut(Request) -> Result<Response, String> + Send + 'static,
    ) -> Self {
        let (requests, request_rx) = mpsc::channel::<Request>();
        let (response_tx, responses) = mpsc::channel();
        render::spawn_worker(
            name,
            move || {
                while let Ok(request) = request_rx.recv() {
                    // A panic fails this request, not the worker: the hook has
                    // recorded it for the status line, and the next request
                    // gets a fresh attempt.
                    let response =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(request)))
                            .unwrap_or_else(|_| Err("the background reader crashed".to_string()));
                    if response_tx.send(response).is_err() {
                        break;
                    }
                }
            },
            || {},
        );
        Self {
            requests,
            responses,
            in_flight: false,
            waiting: None,
        }
    }

    /// Ask for `request` to run: now if the worker is idle, otherwise as soon
    /// as the current one finishes, replacing any request already waiting.
    pub(super) fn request(&mut self, request: Request) {
        if self.in_flight {
            self.waiting = Some(request);
        } else {
            self.send(request);
        }
    }

    /// Whether a request is running or waiting.
    pub(super) fn busy(&self) -> bool {
        self.in_flight || self.waiting.is_some()
    }

    /// The finished request's result, if one is ready, starting the waiting
    /// request (if any) in its place.
    pub(super) fn try_take(&mut self) -> Option<Result<Response, String>> {
        let response = match self.responses.try_recv() {
            Ok(response) => response,
            Err(mpsc::TryRecvError::Empty) => return None,
            // The worker never exits while we hold its sender; treat a lost
            // worker like an idle one so requests are not stuck "in flight".
            Err(mpsc::TryRecvError::Disconnected) => {
                self.in_flight = false;
                self.waiting = None;
                return None;
            }
        };
        self.in_flight = false;
        if let Some(request) = self.waiting.take() {
            self.send(request);
        }
        Some(response)
    }

    fn send(&mut self, request: Request) {
        self.in_flight = self.requests.send(request).is_ok();
    }
}

/// A finished location refresh: the processes enriched with where they are
/// attached in this outer session, and the snapshot the worker had to read
/// itself when asked to (see [`SidebarPoller::request_locations`]).
pub(super) struct Locations {
    pub(super) fetched: Option<Arc<crate::StateSnapshot>>,
    pub(super) processes: Vec<crate::SidebarProcess>,
}

/// The sidebar's two background lanes.
pub(super) struct SidebarPoller {
    pub(super) focus: Lane<(), OuterFocus>,
    pub(super) locations: Lane<Option<Arc<crate::StateSnapshot>>, Locations>,
}

impl SidebarPoller {
    pub(super) fn spawn(context: &crate::SidebarContext) -> Self {
        let focus_context = context.clone();
        let locations_context = context.clone();
        Self {
            focus: Lane::spawn("pmux-focus", move |()| {
                outer_focus(&focus_context).map_err(|err| format!("{err:#}"))
            }),
            locations: Lane::spawn("pmux-locations", move |snapshot| {
                let (snapshot, fetched) = match snapshot {
                    Some(snapshot) => (snapshot, None),
                    None => {
                        let snapshot = Arc::new(crate::state().map_err(|err| format!("{err:#}"))?);
                        (Arc::clone(&snapshot), Some(snapshot))
                    }
                };
                Ok(Locations {
                    fetched,
                    processes: crate::sidebar_processes(&snapshot, &locations_context),
                })
            }),
        }
    }

    /// Re-enrich `snapshot`, or with `None`, read a fresh one first (used
    /// before the event stream has delivered anything, and while it is down).
    pub(super) fn request_locations(&mut self, snapshot: Option<Arc<crate::StateSnapshot>>) {
        self.locations.request(snapshot);
    }

    pub(super) fn busy(&self) -> bool {
        self.focus.busy() || self.locations.busy()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for<T>(lane: &mut Lane<u32, T>) -> Result<T, String>
    where
        T: Send + 'static,
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(response) = lane.try_take() {
                return response;
            }
            assert!(Instant::now() < deadline, "the worker never answered");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_lane_runs_one_request_at_a_time_and_keeps_only_the_newest_waiting() {
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let mut lane = Lane::spawn("pmux-test-lane", move |request: u32| {
            gate_rx.recv().unwrap();
            Ok(request * 10)
        });
        assert!(!lane.busy());
        lane.request(1);
        lane.request(2);
        lane.request(3);
        assert!(lane.busy());
        gate_tx.send(()).unwrap();
        assert_eq!(wait_for(&mut lane), Ok(10));
        // Request 2 was superseded by 3 while 1 ran.
        gate_tx.send(()).unwrap();
        assert_eq!(wait_for(&mut lane), Ok(30));
        assert!(!lane.busy());
        assert!(lane.try_take().is_none());
    }

    #[test]
    fn a_panicking_request_fails_without_stopping_the_lane() {
        let mut lane = Lane::spawn("pmux-test-lane", |request: u32| {
            if request == 0 {
                panic!("simulated reader failure");
            }
            Ok(request)
        });
        lane.request(0);
        assert!(wait_for(&mut lane).is_err());
        lane.request(7);
        assert_eq!(wait_for(&mut lane), Ok(7));
    }
}
