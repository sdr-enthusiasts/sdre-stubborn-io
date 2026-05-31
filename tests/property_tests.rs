//! Property-based tests over the `StubbornIo` state machine and helpers.
//!
//! These exist to surface state-machine corners that ad-hoc tests miss: the
//! crate's promised invariants should hold for any plausible sequence of
//! connect outcomes, retry counts, and write-policy interactions.

#![allow(missing_docs, clippy::missing_panics_doc)]

mod common;

use common::{DummyCtor, DummyIo, Outcome};
use proptest::prelude::*;
use sdre_stubborn_io::ReconnectOptions;
use sdre_stubborn_io::config::WriteFailurePolicy;
use sdre_stubborn_io::tokio::StubbornIo;
use std::io::{ErrorKind, IoSlice};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

type StubbornDummy = StubbornIo<DummyIo>;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn outcomes(bits: &[bool]) -> Vec<Outcome> {
    bits.iter()
        .map(|&b| {
            if b {
                Outcome::Ok
            } else {
                Outcome::Err(ErrorKind::ConnectionRefused)
            }
        })
        .collect()
}

proptest! {
    // If any outcome in the initial-connect sequence is Ok and retries are
    // sufficient to reach it, connect_with_options must succeed and the stream
    // must be in the Connected state. If all are Err and retries are insufficient,
    // it must surface an error.
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn initial_connect_succeeds_iff_ok_within_retries(
        bits in prop::collection::vec(any::<bool>(), 1..8usize),
    ) {
        let total = bits.len();
        let any_ok_in_window = bits.iter().any(|&b| b);
        let ctor = DummyCtor::new(outcomes(&bits));
        let opts = ReconnectOptions::new()
            // Allow exactly enough retries to consume every scripted outcome.
            .with_retries_generator(move || vec![Duration::from_millis(1); total]);

        let result = rt().block_on(StubbornDummy::connect_with_options(ctor, opts));
        if any_ok_in_window {
            prop_assert!(result.is_ok());
            let s = result.unwrap();
            prop_assert!(s.is_connected());
            prop_assert!(!s.is_terminated());
        } else {
            prop_assert!(result.is_err());
        }
    }

    // DropAndNotify vectored writes must always report a written length equal
    // to the sum of the input slice lengths, regardless of slice layout.
    #[test]
    fn drop_and_notify_vectored_reports_sum(
        lens in prop::collection::vec(0usize..32, 1..6usize),
    ) {
        let total: usize = lens.iter().sum();
        let buffers: Vec<Vec<u8>> = lens.iter().map(|&n| vec![0xAB; n]).collect();
        let ctor = DummyCtor::new(vec![Outcome::Ok, Outcome::Ok])
            .with_write_script(vec![Some(std::task::Poll::Ready(Err(
                std::io::Error::new(ErrorKind::BrokenPipe, "peer gone"),
            )))]);
        let opts = ReconnectOptions::new()
            .with_write_failure_policy(WriteFailurePolicy::DropAndNotify)
            .with_retries_generator(|| vec![Duration::from_millis(1); 2]);
        let n = rt().block_on(async move {
            let mut s = StubbornDummy::connect_with_options(ctor, opts).await.unwrap();
            let slices: Vec<IoSlice<'_>> = buffers.iter().map(|b| IoSlice::new(b)).collect();
            s.write_vectored(&slices).await.unwrap()
        });
        prop_assert_eq!(n, total);
    }

    // The connect_timeout knob must turn arbitrarily-slow establishes into a
    // bounded number of retries that ultimately succeed when a fast Ok appears.
    #[test]
    fn connect_timeout_bounds_slow_attempts(
        slow_count in 1usize..4,
    ) {
        let mut script = vec![Outcome::SlowOk(Duration::from_secs(60)); slow_count];
        script.push(Outcome::Ok);
        let total = script.len();
        let ctor = DummyCtor::new(script);
        let opts = ReconnectOptions::new()
            .with_connect_timeout(Some(Duration::from_millis(10)))
            .with_retries_generator(move || vec![Duration::from_millis(1); total]);
        let s = rt().block_on(StubbornDummy::connect_with_options(ctor.clone(), opts)).unwrap();
        prop_assert!(s.is_connected());
        prop_assert_eq!(ctor.establish_count(), slow_count + 1);
    }
}
