//! Property-based test for the Modem Manager's command serialization
//! invariant (Property 27).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/modem.rs`) per the spec's test-placement note. It exercises the real
//! single-owner command loop (`run_session`, reached through the
//! `run_session_with_transport` seam) with an in-memory mock `SerialTransport`
//! that tracks how many AT commands are "in flight" on the port at once.
//!
//! Property 27: At most one AT command outstanding.
//! *For any* number and interleaving of concurrently submitted modem requests,
//! the maximum number of AT commands in flight on the serial port at any
//! instant never exceeds one.
//!
//! Validates: Requirements 8.3

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proptest::prelude::*;

use sms_micro_service::config::{Config, LogLevel};
use sms_micro_service::db::Db;
use sms_micro_service::modem::{new_modem, run_session_with_transport, SerialTransport};

/// Shared, thread-safe tracker of AT-command concurrency on the mock port.
///
/// A command is considered "in flight" from the moment its bytes are written
/// to the port (`write_bytes`) until its terminating result code is read back
/// (`read_line` returning a terminator). `max_in_flight` records the largest
/// number of commands ever simultaneously outstanding, and `started` counts
/// how many command exchanges began so the test can confirm it actually
/// exercised the loop.
#[derive(Default)]
struct TrackerState {
    in_flight: u32,
    max_in_flight: u32,
    started: u64,
}

#[derive(Clone, Default)]
struct ConcurrencyTracker {
    inner: Arc<Mutex<TrackerState>>,
}

impl ConcurrencyTracker {
    /// Mark the start of a command exchange (bytes written to the port).
    fn begin(&self) {
        let mut st = self.inner.lock().expect("tracker poisoned");
        st.in_flight += 1;
        st.started += 1;
        if st.in_flight > st.max_in_flight {
            st.max_in_flight = st.in_flight;
        }
    }

    /// Whether a command is currently outstanding (awaiting its terminator).
    fn is_mid_exchange(&self) -> bool {
        self.inner.lock().expect("tracker poisoned").in_flight > 0
    }

    /// Mark the end of a command exchange (terminator read from the port).
    fn end(&self) {
        let mut st = self.inner.lock().expect("tracker poisoned");
        st.in_flight = st.in_flight.saturating_sub(1);
    }

    fn max_in_flight(&self) -> u32 {
        self.inner.lock().expect("tracker poisoned").max_in_flight
    }

    fn started(&self) -> u64 {
        self.inner.lock().expect("tracker poisoned").started
    }
}

/// An in-memory `SerialTransport` that never talks to real hardware. Every
/// command written is answered with a single `OK` terminator, and the
/// transport records command concurrency through a shared [`ConcurrencyTracker`].
///
/// Small `sleep` await points are inserted on both the write and the read so
/// that, *if* the manager ever (incorrectly) issued a second command before
/// the first completed, the overlap would be observed by `max_in_flight`
/// climbing above one. Because the manager owns the transport by `&mut` and
/// awaits each exchange to completion, no overlap can occur — which is exactly
/// what the property asserts.
struct MockTransport {
    tracker: ConcurrencyTracker,
}

impl SerialTransport for MockTransport {
    async fn write_bytes(&mut self, _data: &[u8]) -> std::io::Result<()> {
        // A command's bytes are now on the wire: it is outstanding.
        self.tracker.begin();
        // Yield to the runtime (without a fixed delay) so any concurrent
        // exchange would have the opportunity to interleave here and be
        // observed by `max_in_flight`.
        tokio::task::yield_now().await;
        Ok(())
    }

    async fn read_line(&mut self, _timeout: Duration) -> std::io::Result<Option<String>> {
        if self.tracker.is_mid_exchange() {
            // Mid-exchange: yield, then answer with a terminating `OK`, which
            // completes the outstanding command.
            tokio::task::yield_now().await;
            self.tracker.end();
            Ok(Some("OK".to_string()))
        } else {
            // Idle URC poll: no unsolicited data ever arrives from this mock.
            // A short real sleep (rather than a busy yield) avoids spinning the
            // session loop while it waits for the next command or shutdown.
            tokio::time::sleep(Duration::from_millis(1)).await;
            Ok(None)
        }
    }
}

/// A minimal valid `Config` for driving the session loop. Only the AT timeout
/// matters for this test; the rest are plausible defaults.
fn test_config() -> Config {
    Config {
        listen_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        serial_port: "mock".to_string(),
        baud_rate: 115_200,
        database_path: ":memory:".to_string(),
        service_center_number: None,
        at_timeout_secs: 1,
        default_rate_limit: 100,
        rate_window_secs: 60,
        log_level: LogLevel::Error,
        reopen_max_attempts: 10,
        send_max_attempts: 3,
        send_retry_delay_secs: 1,
    }
}

/// Drive the real single-owner session loop with the mock transport while
/// `command_count` raw AT commands are submitted concurrently, then return the
/// observed maximum in-flight count and the number of exchanges that ran.
async fn run_scenario(commands: Vec<String>) -> (u32, u64) {
    let tracker = ConcurrencyTracker::default();
    let transport = MockTransport {
        tracker: tracker.clone(),
    };

    // The Raw request path never touches the database, but the loop signature
    // requires a handle; an unmigrated in-memory database is sufficient.
    let db = Db::connect(":memory:")
        .await
        .expect("open in-memory database");

    let (handle, endpoint) = new_modem(commands.len().max(1) + 8);

    // Spawn the manager session; it runs until every handle clone is dropped.
    let session = tokio::spawn(run_session_with_transport(
        test_config(),
        db,
        endpoint,
        transport,
    ));

    // Submit all commands concurrently so their *submission* interleaves
    // arbitrarily; the loop must still serialize their *execution*.
    let mut joins = Vec::with_capacity(commands.len());
    for command in commands {
        let h = handle.clone();
        joins.push(tokio::spawn(async move { h.raw(&command).await }));
    }
    for join in joins {
        let _ = join.await;
    }

    // Drop the last sender so the command channel closes and the loop ends.
    drop(handle);
    let _ = session.await;

    (tracker.max_in_flight(), tracker.started())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 100, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 27: At most one AT command
    // outstanding. For any number and interleaving of concurrently submitted
    // modem requests, the maximum number of AT commands in flight on the
    // serial port at any instant never exceeds one.
    //
    // Validates: Requirements 8.3
    #[test]
    fn prop_at_most_one_at_command_outstanding(
        commands in prop::collection::vec("AT[A-Z+?=0-9]{0,8}", 1..=20),
    ) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("build tokio runtime");

        let (max_in_flight, started) = runtime.block_on(run_scenario(commands));

        // The core invariant (Req 8.3): never more than one command in flight.
        prop_assert!(
            max_in_flight <= 1,
            "observed {} AT commands in flight simultaneously; at most 1 is allowed",
            max_in_flight
        );

        // Sanity: the loop actually ran exchanges (status refresh + the raw
        // commands), so a passing assertion above is meaningful rather than
        // vacuous.
        prop_assert!(
            started >= 1,
            "expected the session loop to run at least one AT exchange, ran {}",
            started
        );
    }
}
