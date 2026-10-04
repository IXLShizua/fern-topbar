use std::{fmt, io::IsTerminal};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::{
    EnvFilter,
    fmt::{FmtContext, FormatEvent, FormatFields, format::Writer},
    registry::LookupSpan,
};

pub fn load() {
    let filter = EnvFilter::builder()
        .with_default_directive(tracing_subscriber::filter::LevelFilter::WARN.into())
        .parse_lossy(
            std::env::var("RUST_LOG")
                .as_deref()
                .unwrap_or("warn,fern_topbar=info"),
        );

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none())
        .event_format(LogFormat)
        .with_writer(std::io::stderr)
        .init();
}

/// Event fields carry the useful context; Relm4's input/component spans stay out of the output.
struct LogFormat;

impl<S, N> FormatEvent<S, N> for LogFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        context: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let metadata = event.metadata();
        let level = metadata.level();

        write!(
            writer,
            "{} ",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f")
        )?;

        if writer.has_ansi_escapes() {
            let color = match *level {
                Level::ERROR => 31,
                Level::WARN => 33,
                Level::INFO => 32,
                Level::DEBUG => 34,
                Level::TRACE => 35,
            };

            write!(writer, "\x1b[{color}m{level:>5}\x1b[0m ")?;
        } else {
            write!(writer, "{level:>5} ")?;
        }

        if *level != Level::INFO {
            write!(writer, "{}: ", metadata.target())?;
        }

        context
            .field_format()
            .format_fields(writer.by_ref(), event)?;

        writeln!(writer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io,
        sync::{Arc, Mutex},
    };

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl io::Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);

            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn keeps_event_fields_without_framework_spans_or_dependency_info() {
        for ansi in [false, true] {
            let output = LogBuffer::default();
            let writer = output.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::new("warn,fern_topbar=info"))
                .with_ansi(ansi)
                .event_format(LogFormat)
                .with_writer(move || writer.clone())
                .finish();

            tracing::subscriber::with_default(subscriber, || {
                let _span = tracing::info_span!(
                    "update_with_view",
                    input = "internal input",
                    component = "PanelContent",
                    id = "0x123"
                )
                .entered();

                tracing::info!(feature = "clock", "feature restored");
                tracing::info!(target: "dependency", "dependency chatter");
                tracing::warn!(target: "dependency", error = "connection lost", "subscription failed");
            });

            let bytes = output.0.lock().unwrap().clone();
            let log = String::from_utf8(bytes).unwrap();
            let plain: String = log
                .split('\x1b')
                .map(|part| {
                    part.strip_prefix('[')
                        .and_then(|escape| escape.split_once('m'))
                        .map_or(part, |(_, text)| text)
                })
                .collect();

            assert_eq!(log.lines().count(), 2);
            assert!(
                plain.contains("feature restored feature=\"clock\""),
                "{log:?}"
            );
            assert!(
                plain.contains("dependency: subscription failed error=\"connection lost\""),
                "{log:?}"
            );
            assert!(!log.contains("update_with_view"));
            assert!(!log.contains("internal input"));
            assert!(!log.contains("PanelContent"));
            assert!(!log.contains("0x123"));
            assert!(!log.contains("dependency chatter"));
            assert_eq!(log.contains('\x1b'), ansi);
        }
    }
}
