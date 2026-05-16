use signal_hook::consts::signal::*;
use signal_hook::iterator::Signals;
use std::any::Any;
use std::error::Error;
use std::io;
use std::process;
use std::sync::mpsc;
use std::thread;

use crate::control;

pub fn run() -> Result<(), Box<dyn Error>> {
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP])?;
    let handle = signals.handle();
    let (shutdown_tx, shutdown_rx) = mpsc::channel();
    let (shutdown_started_tx, shutdown_started_rx) = mpsc::channel();

    let signal_thread = thread::spawn(move || {
        let mut shutdown_started = false;

        for s in &mut signals {
            if shutdown_started {
                eprintln!(
                    "received second shutdown signal {}; exiting immediately",
                    signal_name(s)
                );
                if let Err(err) = control::resume_before_forced_exit() {
                    eprintln!("failed to resume target before forced exit: {err}");
                }
                process::exit(signal_exit_code(s));
            }

            shutdown_started = true;
            let _ = shutdown_started_tx.send(());
            println!(
                "received shutdown signal {}; stopping control loop and exiting",
                signal_name(s)
            );

            let shutdown_tx = shutdown_tx.clone();
            thread::spawn(move || {
                let shutdown_result = control::stop();
                if let Err(err) = &shutdown_result {
                    eprintln!("failed to stop control loop cleanly: {err}");
                }
                let _ = shutdown_tx.send(shutdown_result);
            });
        }
    });

    let control_result = control::start();

    handle.close();

    if let Err(payload) = signal_thread.join() {
        return Err(Box::new(signal_thread_panic_error(
            payload.as_ref(),
            &control_result,
        )));
    }

    let shutdown_result = match shutdown_started_rx.try_recv() {
        Ok(()) => shutdown_rx
            .recv()
            .map_err(|err| -> Box<dyn Error> { Box::new(err) })?
            .map_err(|err| -> Box<dyn Error> { Box::new(err) }),
        Err(_) => Ok(()),
    };

    control_result?;
    shutdown_result
}

fn signal_name(signal: i32) -> &'static str {
    match signal {
        SIGINT => "SIGINT",
        SIGTERM => "SIGTERM",
        SIGHUP => "SIGHUP",
        _ => "UNKNOWN",
    }
}

fn signal_exit_code(signal: i32) -> i32 {
    128 + signal
}

fn panic_payload_suffix(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        format!(": {message}")
    } else if let Some(message) = payload.downcast_ref::<String>() {
        format!(": {message}")
    } else {
        String::new()
    }
}

fn signal_thread_panic_error(
    payload: &(dyn Any + Send),
    control_result: &Result<(), Box<dyn Error>>,
) -> io::Error {
    let mut message = format!(
        "signal-handling thread panicked{}",
        panic_payload_suffix(payload)
    );
    if let Err(err) = control_result {
        message.push_str(&format!("; control loop also failed: {err}"));
    }
    io::Error::other(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed_panic_payload(payload: impl Any + Send) -> Box<dyn Any + Send> {
        Box::new(payload)
    }

    #[test]
    fn maps_supported_shutdown_signals_to_names() {
        assert_eq!(signal_name(SIGINT), "SIGINT");
        assert_eq!(signal_name(SIGTERM), "SIGTERM");
        assert_eq!(signal_name(SIGHUP), "SIGHUP");
    }

    #[test]
    fn maps_signal_to_forced_shutdown_exit_code() {
        assert_eq!(signal_exit_code(SIGINT), 130);
        assert_eq!(signal_exit_code(SIGTERM), 143);
    }

    #[test]
    fn includes_str_panic_payload_in_suffix() {
        let payload = boxed_panic_payload("signal loop failed");

        assert_eq!(
            panic_payload_suffix(payload.as_ref()),
            ": signal loop failed"
        );
    }

    #[test]
    fn includes_string_panic_payload_in_suffix() {
        let payload = boxed_panic_payload(String::from("signal loop failed"));

        assert_eq!(
            panic_payload_suffix(payload.as_ref()),
            ": signal loop failed"
        );
    }

    #[test]
    fn omits_unknown_panic_payload_from_suffix() {
        let payload = boxed_panic_payload(42_u8);

        assert!(panic_payload_suffix(payload.as_ref()).is_empty());
    }

    #[test]
    fn signal_thread_panic_error_includes_control_loop_error() {
        let payload = boxed_panic_payload("signal loop failed");
        let control_result: Result<(), Box<dyn Error>> =
            Err(Box::new(io::Error::other("control loop failed")));

        let message = signal_thread_panic_error(payload.as_ref(), &control_result).to_string();

        assert!(message.contains("signal-handling thread panicked: signal loop failed"));
        assert!(message.contains("control loop also failed: control loop failed"));
    }

    #[test]
    fn signal_thread_panic_error_omits_successful_control_loop() {
        let payload = boxed_panic_payload("signal loop failed");
        let control_result = Ok(());

        let message = signal_thread_panic_error(payload.as_ref(), &control_result).to_string();

        assert_eq!(
            message,
            "signal-handling thread panicked: signal loop failed"
        );
    }
}
