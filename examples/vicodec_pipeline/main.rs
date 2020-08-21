mod encoder;
mod framegen;

use encoder::*;
use framegen::FrameGenerator;

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::{cell::RefCell, collections::VecDeque, time::Instant};

use anyhow::ensure;

use clap::{App, Arg};
use v4l2::{
    device::queue::{direction::Capture, dqbuf::DQBuffer},
    memory::MMAP,
};

const FRAME_SIZE: (usize, usize) = (640, 480);

fn main() {
    let matches = App::new("vicodec example")
        .arg(
            Arg::with_name("num_frames")
                .long("stop_after")
                .takes_value(true)
                .help("Stop after encoding a given number of buffers"),
        )
        .arg(
            Arg::with_name("device")
                .required(true)
                .help("Path to the vicodec device file"),
        )
        .arg(
            Arg::with_name("output_file")
                .long("save")
                .required(false)
                .takes_value(true)
                .help("Save the encoded stream to a file"),
        )
        .get_matches();

    let device_path = matches.value_of("device").unwrap_or("/dev/video0");

    let mut stop_after = match clap::value_t!(matches.value_of("num_frames"), usize) {
        Ok(v) => Some(v),
        Err(e) if e.kind == clap::ErrorKind::ArgumentNotFound => None,
        Err(e) => panic!("Invalid value for stop_after: {}", e),
    };

    let mut output_file: Option<File> = if let Some(path) = matches.value_of("output_file") {
        Some(File::create(path).expect("Invalid output file specified."))
    } else {
        None
    };

    let lets_quit = Arc::new(AtomicBool::new(false));
    // Setup the Ctrl+c handler.
    {
        let lets_quit_handler = lets_quit.clone();
        ctrlc::set_handler(move || {
            lets_quit_handler.store(true, Ordering::SeqCst);
        })
        .expect("Failed to set Ctrl-C handler.");
    }

    let encoder = Encoder::open(&Path::new(&device_path)).expect("Failed to open device");
    let encoder = encoder
        .set_capture_format(|f| {
            let format = f.set_pixelformat(b"FWHT").apply()?;

            ensure!(
                format.pixelformat == b"FWHT".into(),
                "FWHT format not supported"
            );

            Ok(())
        })
        .expect("Failed to set capture format");

    let encoder = encoder
        .set_output_format(|f| {
            let format = f
                .set_pixelformat(b"RGB3")
                .set_size(FRAME_SIZE.0, FRAME_SIZE.1)
                .apply()?;

            ensure!(
                format.pixelformat == b"RGB3".into(),
                "RGB3 format not supported"
            );
            ensure!(
                format.width as usize == FRAME_SIZE.0 && format.height as usize == FRAME_SIZE.1,
                "Output frame resolution not supported"
            );

            Ok(())
        })
        .expect("Failed to set output format");

    let output_format = encoder
        .get_output_format()
        .expect("Failed to get output format");
    println!("Adjusted output format: {:?}", output_format);
    println!(
        "Adjusted capture format: {:?}",
        encoder
            .get_capture_format()
            .expect("Failed to get capture format")
    );

    println!(
        "Configured encoder for {}x{} ({} bytes per line)",
        output_format.width, output_format.height, output_format.plane_fmt[0].bytesperline
    );

    let mut frame_gen = FrameGenerator::new(
        output_format.width as usize,
        output_format.height as usize,
        output_format.plane_fmt[0].bytesperline as usize,
    )
    .expect("Failed to create frame generator");

    const NUM_BUFFERS: usize = 2;

    let free_buffers: VecDeque<_> =
        std::iter::repeat(vec![0u8; output_format.plane_fmt[0].sizeimage as usize])
            .take(NUM_BUFFERS)
            .collect();
    let free_buffers = RefCell::new(free_buffers);

    let input_done_cb = |handles: &mut Vec<Vec<u8>>| {
        free_buffers.borrow_mut().push_back(handles.remove(0));
    };

    let mut total_size = 0usize;
    let start_time = Instant::now();
    let output_ready_cb = move |cap_dqbuf: DQBuffer<Capture, MMAP>| {
        let bytes_used = cap_dqbuf.data.planes[0].bytesused as usize;
        total_size = total_size.wrapping_add(bytes_used);
        let elapsed = start_time.elapsed();
        let frame_nb = cap_dqbuf.data.sequence + 1;
        let fps = frame_nb as f32 / elapsed.as_millis() as f32 * 1000.0;
        //let num_poll_wakeups = encoder.get_num_poll_wakeups();
        print!(
            "\rEncoded buffer {:#5}, index: {:#2}), bytes used:{:#6} total encoded size:{:#8} fps: {:#5.2}",
            cap_dqbuf.data.sequence,
            cap_dqbuf.data.index,
            bytes_used,
            total_size,
            fps,
        );
        io::stdout().flush().unwrap();

        if let Some(ref mut output) = output_file {
            let mapping = cap_dqbuf
                .get_plane_mapping(0)
                .expect("Failed to map capture buffer");
            output
                .write_all(mapping.as_ref())
                .expect("Error while writing output data");
        }
    };

    let encoder = encoder
        .allocate_buffers(NUM_BUFFERS, NUM_BUFFERS)
        .expect("Failed to allocate encoder buffers");
    let encoder = encoder
        .start_encoding(input_done_cb, output_ready_cb)
        .expect("Failed to start encoder");

    while !lets_quit.load(Ordering::SeqCst) {
        if let Some(v4l2_buffer) = encoder.get_buffer() {
            if let Some(mut buffer) = free_buffers.borrow_mut().pop_front() {
                frame_gen
                    .next_frame(&mut buffer[..])
                    .expect("Failed to generate frame");
                let bytes_used = buffer.len();
                // TODO handle the QueueError?
                v4l2_buffer.add_plane(buffer, bytes_used).queue().unwrap();
            }
        }

        if let Some(max_cpt) = &mut stop_after {
            if *max_cpt <= 1 {
                lets_quit.store(true, Ordering::SeqCst);
                break;
            }
            *max_cpt -= 1;
        }
    }

    encoder.stop().unwrap();

    // Insert new line since we were overwriting the same one
    println!();
}
