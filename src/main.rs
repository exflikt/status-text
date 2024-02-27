use std::ffi::OsStr;
use std::future::Future;
use std::num::{NonZeroU64, NonZeroU8};
use std::pin::Pin;
use tokio::io::AsyncWriteExt;
use tokio::signal::unix::Signal;
use tokio::time::Interval;

async fn run_command<S, I>(program: S, args: I) -> Vec<u8>
where
    S: AsRef<OsStr>,
    I: IntoIterator<Item = S>,
{
    tokio::process::Command::new(program.as_ref())
        .args(args)
        .output()
        .await
        .unwrap_or_else(|_| panic!("failed to execute `{}`", program.as_ref().to_string_lossy()))
        .stdout
}

/// Strip the last character of a given byte vector and convert it to a string.
///
/// The last character is assumed to be eol, i.e. a new line character.
fn bytes_to_eol_stripped_string(v: &[u8]) -> Option<String> {
    v.split_last()
        .map(|(_new_line, body)| body)
        .map(String::from_utf8_lossy)
        .map(|cow| cow.into_owned())
}

const STATUS_DELIMITER: &[u8] = " | ".as_bytes();

trait Block {
    fn status_update(&mut self) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send + '_>>;
    fn status_view(&self) -> Vec<u8>;
}

// [ \"$(nmcli --terse general status | cut -d: -f1)\" = 'connected' ] && echo 'Internet ✓' || echo 'No Internet'
#[derive(Default)]
struct InternetConnection {
    up: Option<bool>,
}

impl Block for InternetConnection {
    fn status_update(
        &mut self,
    ) -> Pin<Box<dyn futures::Future<Output = Result<(), ()>> + std::marker::Send + '_>> {
        Box::pin(async {
            let output = run_command("nmcli", ["--terse", "general", "status"]).await;
            self.up = output
                .split(|&byte| byte == b':')
                .next()
                .and_then(|first_field| match first_field {
                    b"connected" => Some(true),
                    b"connecting" | b"disconnected" | b"asleep" => Some(false),
                    _ => None,
                });
            Ok(())
        })
    }

    fn status_view(&self) -> Vec<u8> {
        self.up
            .map(|up| if up { "Internet ✓'" } else { "No Internet" })
            .unwrap_or("N/A")
            .as_bytes()
            .to_vec()
    }
}

// echo \"$([ \"$(pamixer --get-mute)\" = true ] && echo 🔇 || echo 🔊) $(pamixer --get-volume)\"
#[derive(Default)]
struct Audio {
    is_mute: Option<bool>,
    volume: Option<u8>,
}

impl Block for Audio {
    fn status_update(
        &mut self,
    ) -> Pin<Box<dyn futures::Future<Output = Result<(), ()>> + std::marker::Send + '_>> {
        use std::iter::once;

        Box::pin(async {
            let mute_status = run_command("pamixer", once("--get-mute")).await;
            self.is_mute = bytes_to_eol_stripped_string(mute_status.as_slice())
                .map(|mute_status| match mute_status.as_ref() {
                    "true" => true,
                    "false" => false,
                    v => panic!("Unexpected output: {v} [pamixer has changed the output to produce values other than true or false]"),
                });
            let volume = run_command("pamixer", once("--get-volume")).await;
            self.volume = bytes_to_eol_stripped_string(volume.as_slice())
                .and_then(|volume| volume.parse::<u8>().ok());
            Ok(())
        })
    }

    fn status_view(&self) -> Vec<u8> {
        let mute_status = self
            .is_mute
            .map(|is_mute| if is_mute { "🔇" } else { "🔊" })
            .unwrap_or("N/A");
        self.volume
            .map(|v| format!("{mute_status} {v}"))
            .unwrap_or(format!("{mute_status} N/A"))
            .as_bytes()
            .to_vec()
    }
}

// sudo ddcutil --sleep-multiplier 0.6 getvcp 10 -t | awk '{print $4\"/\"$5}' &
#[derive(Default)]
struct DisplayBrightness {
    brightness: Option<u8>,
}

impl Block for DisplayBrightness {
    fn status_update(
        &mut self,
    ) -> Pin<Box<dyn futures::Future<Output = Result<(), ()>> + std::marker::Send + '_>> {
        Box::pin(async {
            let output = run_command(
                "ddcutil",
                ["--sleep-multiplier", "0.6", "getvcp", "10", "-t"],
            )
            .await;
            self.brightness = output
                .as_slice()
                .split(|&byte| byte == b' ')
                .nth(3)
                .map(String::from_utf8_lossy)
                .map(|field| field.parse::<u8>())
                .map(|parsed| {
                    parsed.expect("The brightness number is expected to be within 0 - 100")
                });
            Ok(())
        })
    }

    fn status_view(&self) -> Vec<u8> {
        self.brightness
            .map(|brightness| format!("{}/100", brightness))
            .unwrap_or("N/A".to_string())
            .as_bytes()
            .to_vec()
    }
}

// df --human-readable | awk '/^\\/dev\\/sda/ {print $3 \"/\" $2}'
#[derive(Default)]
struct DiskUsage {
    size: Option<String>,
    used: Option<String>,
}

impl Block for DiskUsage {
    fn status_update(
        &mut self,
    ) -> Pin<Box<dyn futures::Future<Output = Result<(), ()>> + std::marker::Send + '_>> {
        Box::pin(async {
            let output = run_command("df", std::iter::once("--human-readable")).await;
            let output = String::from_utf8_lossy(&output);
            let mut iter = output
                .lines()
                .find(|line| line.starts_with("/dev/sda2"))
                .expect("/dev/sda2 device does not exist")
                .split_ascii_whitespace()
                .skip(1);
            self.size = iter.next().map(ToString::to_string);
            self.used = iter.next().map(ToString::to_string);
            Ok(())
        })
    }

    fn status_view(&self) -> Vec<u8> {
        let [used, size] = [&self.used, &self.size].map(|x| x.as_deref().unwrap_or("N/A"));
        format!("{used} / {size}").as_bytes().to_vec()
    }
}

// free --human | awk '/^Mem/ { print $3\"/\"$2 }' | sed s/i//g"
#[derive(Default)]
struct MemoryUsage {
    total: Option<String>,
    used: Option<String>,
}

impl Block for MemoryUsage {
    fn status_update(
        &mut self,
    ) -> Pin<Box<dyn futures::Future<Output = Result<(), ()>> + std::marker::Send + '_>> {
        Box::pin(async {
            let output = run_command("free", std::iter::once("--human")).await;
            let binding = String::from_utf8_lossy(&output);
            let mut iter = binding
                .lines()
                .find(|line| line.starts_with("Mem"))
                .expect("Mem row is not reported by `free`")
                .split_ascii_whitespace()
                .skip(1);
            self.total = iter.next().map(ToString::to_string);
            self.used = iter.next().map(ToString::to_string);
            Ok(())
        })
    }

    fn status_view(&self) -> Vec<u8> {
        let [used, total] = [&self.used, &self.total].map(|x| x.as_deref().unwrap_or("N/A"));
        format!("{used} / {total}").as_bytes().to_vec()
    }
}

// date '+%b %d %a %I:%M%p'
#[derive(Default)]
struct DateTime {}

impl Block for DateTime {
    fn status_update(
        &mut self,
    ) -> Pin<Box<dyn futures::Future<Output = Result<(), ()>> + std::marker::Send>> {
        Box::pin(async { Ok(()) })
    }

    fn status_view(&self) -> Vec<u8> {
        use chrono::Local;
        Local::now().format("%m/%d %H:%M").to_string().into_bytes()
    }
}

struct StatusLineBuilder<'a, const N: usize> {
    blocks: [&'a mut (dyn Block + 'a); N],
    intervals: [Option<NonZeroU64>; N],
    signums: [Option<NonZeroU8>; N],
}

impl<'a, const N: usize> StatusLineBuilder<'a, N> {
    fn new(array: [(&'a mut dyn Block, Option<NonZeroU64>, Option<NonZeroU8>); N]) -> Self {
        let intervals = std::array::from_fn(|i| array[i].1);
        let signums = std::array::from_fn(|i| array[i].2);
        let blocks = array.map(|(block, _, _)| block);

        StatusLineBuilder {
            blocks,
            intervals,
            signums,
        }
    }

    fn build(
        self,
    ) -> (
        StatusLine<'a, N>,
        [Option<Interval>; N],
        [Option<Signal>; N],
    ) {
        let interval_streams = std::array::from_fn(|idx| {
            self.intervals[idx]
                .map(|int| tokio::time::interval(std::time::Duration::from_millis(int.get())))
        });

        use tokio::signal::unix::{signal, SignalKind};
        let signal_streams = std::array::from_fn(|idx| {
            self.signums[idx].map(|signum| {
                signal(SignalKind::from_raw(signum.get().into()))
                    .expect("Could not establish a signal hook")
            })
        });

        (
            StatusLine::new(self.blocks),
            interval_streams,
            signal_streams,
        )
    }
}

struct StatusLine<'a, const N: usize> {
    blocks: [&'a mut dyn Block; N],
    texts: [Option<Vec<u8>>; N],
}

impl<'a, const N: usize> StatusLine<'a, N> {
    fn new(blocks: [&'a mut dyn Block; N]) -> Self {
        const INIT_TEXT: Option<Vec<u8>> = None;
        StatusLine {
            blocks,
            texts: [INIT_TEXT; N],
        }
    }

    async fn update(&mut self, idx: usize) -> Result<(), ()> {
        match self.blocks[idx].status_update().await {
            Ok(_) => {
                let updated_text = Some(self.blocks[idx].status_view());
                if self.texts[idx] != updated_text {
                    self.texts[idx] = updated_text;
                    Ok(())
                } else {
                    Err(())
                }
            }
            v => v,
        }
    }

    async fn update_all(&mut self) {
        use futures::future::join_all;
        join_all(self.blocks.iter_mut().map(|block| block.status_update())).await;

        for idx in 0..N {
            let updated_text = Some(self.blocks[idx].status_view());
            if self.texts[idx] != updated_text {
                self.texts[idx] = updated_text;
            }
        }
    }

    fn view(&self) -> Vec<u8> {
        // To avoid extra allocation, allocate 5 characters per block.
        let mut bytes = Vec::with_capacity(N * 5);

        let mut iter = self.texts.iter();
        if let Some(text) = iter.next() {
            bytes.extend(text.as_deref().unwrap_or("EMPTY".as_bytes()));

            for text in iter {
                bytes.extend(STATUS_DELIMITER);
                bytes.extend(text.as_deref().unwrap_or("EMPTY".as_bytes()));
            }
        }
        bytes.push(b'\n');

        bytes
    }
}

async fn handle_intervals(interval_streams: &mut [Option<Interval>]) -> usize {
    let iter = interval_streams
        .iter_mut()
        .enumerate()
        .flat_map(|(idx, stream)| {
            stream.as_mut().map(|stream| {
                Box::pin(async move {
                    stream.tick().await;
                    idx
                })
            })
        });
    futures::future::select_all(iter).await.0
}

async fn handle_signals(signal_streams: &mut [Option<Signal>]) -> usize {
    let iter = signal_streams
        .iter_mut()
        .enumerate()
        .flat_map(|(idx, stream)| {
            stream.as_mut().map(|stream| {
                Box::pin(async move {
                    stream.recv().await;
                    idx
                })
            })
        });
    futures::future::select_all(iter).await.0
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut audio = Audio::default();
    // let mut internet_connection = InternetConnection::default();
    let mut display_brightness = DisplayBrightness::default();
    let mut disk_usage = DiskUsage::default();
    let mut memory_usage = MemoryUsage::default();
    let mut datetime = DateTime::default();

    let sl_builder = StatusLineBuilder::new([
        // (&mut internet_connection, NonZeroU64::new(30 * 1000), None),
        (&mut audio, None, NonZeroU8::new(50)),
        (&mut display_brightness, None, NonZeroU8::new(55)),
        (&mut disk_usage, NonZeroU64::new(30 * 1000), None),
        (&mut memory_usage, NonZeroU64::new(30 * 1000), None),
        (&mut datetime, NonZeroU64::new(60 * 1000), None),
    ]);
    let (mut sl, mut interval_streams, mut signal_streams) = sl_builder.build();

    eprintln!("process id = {}", std::process::id());

    sl.update_all().await;
    let mut stdout = tokio::io::stdout();
    stdout.write_all(sl.view().as_slice()).await?;
    stdout.flush().await?;

    use tokio::signal::unix::{signal, SignalKind};
    let mut sigusr1 = signal(SignalKind::user_defined1()).expect("Could not establish SIGUSR1");

    loop {
        tokio::select! {
            idx = handle_intervals(interval_streams.as_mut_slice()) => {
                if sl.update(idx).await.is_ok() {
                    stdout.write_all(sl.view().as_slice()).await?;
                    stdout.flush().await?;
                }
            }
            idx = handle_signals(signal_streams.as_mut_slice()) => {
                if sl.update(idx).await.is_ok() {
                    stdout.write_all(sl.view().as_slice()).await?;
                    stdout.flush().await?;
                }
            }
            _ = sigusr1.recv() => {
                println!("INFO: custom message");
            }
        };
    }
}
