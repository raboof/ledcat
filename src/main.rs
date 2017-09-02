extern crate byteorder;
extern crate clap;
#[macro_use]
extern crate derive_error;
#[macro_use]
extern crate ioctl;
extern crate libc;
extern crate nix;
extern crate regex;

use std::borrow::Borrow;
use std::collections;
use std::fs;
use std::io::Write;
use std::io;
use std::net;
use std::ops::Deref;
use std::path;
use std::str::FromStr;
use std::sync;
use std::thread;
use std::time;
use regex::Regex;
use color::*;
use device::*;
use driver::*;
use input::*;
use input::geometry::Transposition;

mod color;
mod device;
mod driver;
mod input;


macro_rules! regex_validator {
    ($expression:expr) => ({
        let ex = Regex::new($expression).unwrap();
        move |val: String| {
            if ex.is_match(val.as_str()) {
                Ok(())
            } else {
                Err(format!("\"{}\" does not match {}", val, ex))
            }
        }
    })
}

fn main() {
    let mut cli = clap::App::new("ledcat")
        .version("0.0.1")
        .author("polyfloyd <floyd@polyfloyd.net>")
        .about("Like netcat, but for leds.")
        .arg(clap::Arg::with_name("output")
            .short("o")
            .long("output")
            .takes_value(true)
            .default_value("-")
            .help("The output file to write to. Use - for stdout."))
        .arg(clap::Arg::with_name("input")
            .short("i")
            .long("input")
            .takes_value(true)
            .min_values(1)
            .multiple(true)
            .default_value("-")
            .help("The inputs to read from. Read the manual for how inputs are read and \
                   prioritized."))
        .arg(clap::Arg::with_name("linger")
            .short("l")
            .long("linger")
            .help("Keep trying to read from the input(s) after EOF is reached"))
        .arg(clap::Arg::with_name("async")
            .long("async")
            .requires("framerate")
            .help("Instead of synchronously reading from one input at a time, consume all data \
                   concurrently, possibly dropping frames."))
        .arg(clap::Arg::with_name("num-pixels")
            .short("n")
            .long("num-pixels")
            .global(true)
            .takes_value(true)
            .validator(regex_validator!(r"^[1-9]\d*$"))
            .help("The number of pixels in the string"))
        .arg(clap::Arg::with_name("geometry")
            .short("g")
            .long("geometry")
            .takes_value(true)
            .conflicts_with("num-pixels")
            .validator(regex_validator!(r"^[1-9]\d*x[1-9]\d*$"))
            .help("Specify the size of a two dimensional display"))
        .arg(clap::Arg::with_name("transpose")
            .short("t")
            .long("transpose")
            .takes_value(true)
            .min_values(1)
            .multiple(true)
            .possible_values(&["reverse", "zigzag_x", "zigzag_y"])
            .help("Apply one or more transpositions to the output"))
        .arg(clap::Arg::with_name("color-correction")
            .short("c")
            .long("color-correction")
            .takes_value(true)
            .possible_values(&["none", "srgb"])
            .help("Override the default color correction. The default is determined per device."))
        .arg(clap::Arg::with_name("dim")
            .long("dim")
            .takes_value(true)
            .default_value("1.0")
            .validator(|v| {
                let f = v.parse::<f32>()
                    .map_err(|e| format!("{}", e))?;
                if 0.0 <= f && f <= 1.0 {
                    Ok(())
                } else {
                    Err(format!("dim value out of range: {}", f))
                }
            })
            .help("Apply a global grayscale before the collor correction. The value should be \
                   between 0 and 1.0 inclusive"))
        .arg(clap::Arg::with_name("driver")
            .long("driver")
            .takes_value(true)
            .help("The driver to use for the output. If this is not specified, the driver is \
                   automaticaly detected based on the output"))
        .arg(clap::Arg::with_name("spidev-clock")
            .long("spidev-clock")
            .takes_value(true)
            .validator(regex_validator!(r"^[1-9]\d*$"))
            .default_value("500000")
            .help("If spidev is used as driver, use this to set the clock frequency in Hertz"))
        .arg(clap::Arg::with_name("serial-baudrate")
            .long("serial-baudrate")
            .takes_value(true)
            .validator(regex_validator!(r"^[1-9]\d*$"))
            .default_value("12000000")
            .help("If serial is used as driver, use this to set the baudrate"))
        .arg(clap::Arg::with_name("framerate")
            .short("f")
            .long("framerate")
            .takes_value(true)
            .validator(regex_validator!(r"^[1-9]\d*$"))
            .help("Limit the number of frames per second"))
        .arg(clap::Arg::with_name("single-frame")
            .short("1")
            .long("one")
            .conflicts_with("framerate")
            .help("Send a single frame to the output and exit"))
        .subcommand(clap::SubCommand::with_name("artnet")
            .about("Control artnet DMX nodes via unicast and broadcast")
            .arg(clap::Arg::with_name("target")
                .short("t")
                .long("target")
                .takes_value(true)
                .min_values(1)
                .multiple(true)
                .validator(|addr| match net::IpAddr::from_str(addr.as_str()) {
                    Ok(_) => Ok(()),
                    Err(err) => Err(format!("{} ({})", err, addr)),
                })
                .conflicts_with_all(&["discover", "target-list", "broadcast"])
                .help("One or more target IP addresses"))
            .arg(clap::Arg::with_name("target-list")
                .long("target-list")
                .takes_value(true)
                .conflicts_with_all(&["target", "discover", "broadcast"])
                .help("Specify a file containing 1 IP address per line to unicast to. \
                       Changes to the file are read automatically"))
            .arg(clap::Arg::with_name("broadcast")
                .short("b")
                .long("broadcast")
                .conflicts_with_all(&["target", "target-list", "discover"])
                .help("Broadcast to all devices in the network"))
            .arg(clap::Arg::with_name("discover")
                .short("d")
                .long("discover")
                .conflicts_with_all(&["target", "target-list", "broadcast"])
                .help("Discover artnet nodes")));

    let mut device_constructors = collections::HashMap::new();
    for device_init in device::devices() {
        device_constructors.insert(device_init.0.get_name().to_string(), device_init.1);
        cli = cli.subcommand(device_init.0);
    }

    let matches = cli.clone().get_matches();
    let (sub_name, sub_matches) = matches.subcommand();
    if sub_name == "" {
        let mut out = io::stderr();
        cli.write_help(&mut out).unwrap();
        eprintln!();
        return;
    }

    if sub_name == "artnet" && sub_matches.unwrap().is_present("discover") {
        if let Err(err) = artnet_discover() {
            eprintln!("{}", err);
        }
        return;
    }

    let dimensions = if let Some(npix) = matches.value_of("num-pixels") {
        geometry::Dimensions::One(npix.parse::<usize>().unwrap())
    } else if let Some(geom) = matches.value_of("geometry") {
        let parsed: Vec<usize> = geom.split('x')
            .map(|d| -> usize { d.parse::<usize>().unwrap() })
            .collect();
        geometry::Dimensions::Two(parsed[0], parsed[1])
    } else {
        eprintln!("Please set the frame size through either --num-pixels or --geometry");
        return;
    };

    let (output, dev) = if sub_name == "artnet" {
        let sub_matches = sub_matches.unwrap();
        let dev: Box<Device> = Box::new(device::generic::Generic {
            clock_phase: 0,
            clock_polarity: 0,
            first_bit: FirstBit::MSB,
        });
        let artnet_target: Box<artnet::Target> = if sub_matches.is_present("broadcast") {
            Box::new(artnet::Broadcast{})
        } else if let Some(list_path) = sub_matches.value_of("target-list") {
            Box::new(artnet::ListFile::new(list_path))
        } else {
            let addresses: Vec<_> = sub_matches.values_of("target").unwrap().map(|addr| {
                net::SocketAddr::new(net::IpAddr::from_str(addr).unwrap(), artnet::PORT)
            }).collect();
            Box::new(addresses)
        };
        let output: Box<io::Write> = match artnet::Unicast::to(artnet_target,
                                                               dimensions.size() * 3) {
            Ok(out) => Box::new(out),
            Err(err) => {
                eprintln!("{}", err);
                return;
            }
        };
        (output, dev)

    } else {
        let dev = device_constructors[sub_name](sub_matches.unwrap());
        let output_file = path::PathBuf::from(match matches.value_of("output").unwrap() {
            "-" => "/dev/stdout",
            _ => matches.value_of("output").unwrap(),
        });

        let driver_name = matches.value_of("driver")
            .map(|s: &str| s.to_string())
            .or(driver::detect(&output_file));
        let driver_name = match driver_name {
            Some(n) => n,
            None => {
                eprintln!("Unable to determine the driver to use. Please set one using --driver.");
                return;
            }
        };
        let output: Box<io::Write> = match driver_name.as_str() {
            "none" => Box::new(fs::OpenOptions::new().write(true).open(&output_file).unwrap()),
            "spidev" => {
                let clock = matches.value_of("spidev-clock").unwrap().parse::<u32>().unwrap();
                Box::new(spidev::open(&output_file, dev.borrow(), clock).unwrap())
            },
            "serial" => {
                let baudrate = matches.value_of("serial-baudrate").unwrap().parse::<u32>().unwrap();
                Box::new(serial::open(&output_file, baudrate).unwrap())
            },
            _ => {
                eprintln!("Unknown driver {}", driver_name);
                return;
            }
        };
        (output, dev)
    };

    let transpose = matches.values_of("transpose")
        .map(|v| v.collect())
        .unwrap_or(vec![]);
    let transposition = match transposition_table(&dimensions, transpose) {
        Ok(t) => t,
        Err(err) => {
            eprintln!("{}", err);
            return;
        }
    };

    let color_correction = matches.value_of("color-correction")
        .and_then(|name| match name {
            "none" => Some(Correction::none()),
            "srgb" => Some(Correction::srgb(255, 255, 255)),
            _ => None,
        })
        .unwrap_or_else(|| dev.color_correction());
    let dim = (matches.value_of("dim")
            .unwrap()
            .parse::<f32>()
            .unwrap() * 255.0)
        .round() as u8;

    let frame_interval = matches.value_of("framerate")
        .map(|fps| time::Duration::new(1, 0) / fps.parse::<u32>().unwrap());
    let single_frame = matches.is_present("single-frame");

    let inputs = matches.values_of("input").unwrap();
    let input_consume = if matches.is_present("async") {
        select::Consume::All(frame_interval.unwrap())
    } else {
        select::Consume::Single
    };
    let input_eof = if matches.is_present("linger") {
        select::WhenEOF::Retry
    } else {
        select::WhenEOF::Close
    };
    let files = inputs.map(|f| match f {
            "-" => "/dev/stdin",
            f => f,
        })
        .collect();
    let mut input =
        select::Reader::from_files(files, dimensions.size() * 3, input_consume, input_eof).unwrap();

    let mut output = io::BufWriter::with_capacity(dev.written_frame_size(dimensions.size()),
                                                  output);

    if single_frame {
        let _ = pipe_frame(&mut input,
                           &mut output,
                           dev.deref(),
                           dimensions.size(),
                           &transposition,
                           &color_correction,
                           dim);
    } else {
        loop {
            let start = time::Instant::now();
            if let Err(_) = pipe_frame(&mut input,
                                       &mut output,
                                       dev.deref(),
                                       dimensions.size(),
                                       &transposition,
                                       &color_correction,
                                       dim) {
                break;
            }
            if let Some(interval) = frame_interval {
                let el = start.elapsed();
                if interval >= el {
                    thread::sleep(interval - el);
                }
            }
        }
    }
}

fn pipe_frame(mut input: &mut io::Read,
              mut output: &mut io::Write,
              dev: &Device,
              num_pixels: usize,
              transposition: &[usize],
              correction: &Correction,
              dim: u8)
              -> io::Result<()> {
    // Read a full frame into a buffer. This prevents half frames being written to a
    // potentially timing sensitive output if the input blocks and lets us apply the
    // transpositions.
    let mut buffer = Vec::new();
    buffer.resize(num_pixels, Pixel { r: 0, g: 0, b: 0 });
    for i in 0..num_pixels {
        let pix_in = Pixel::read_rgb24(&mut input)?;
        let pix_dimmed = {
            let dim16 = dim as u16;
            Pixel {
                r: ((pix_in.r as u16 * dim16) / 0xff) as u8,
                g: ((pix_in.g as u16 * dim16) / 0xff) as u8,
                b: ((pix_in.b as u16 * dim16) / 0xff) as u8,
            }
        };
        let pix_corrected = correction.correct(pix_dimmed);
        buffer[transposition[i]] = pix_corrected;
    }
    dev.write_frame(&mut output, &buffer)?;
    output.flush()
}

fn transposition_table(dimensions: &geometry::Dimensions,
                       operations: Vec<&str>)
                       -> Result<Vec<usize>, String> {
    let transpositions: Vec<Box<geometry::Transposition>> = try!(operations.into_iter()
        .map(|name| -> Result<Box<geometry::Transposition>, String> {
            match name {
                "reverse" => Ok(Box::from(geometry::Reverse { length: dimensions.size() })),
                "zigzag_x" | "zigzag_y" => {
                    let (w, h) = match *dimensions {
                        geometry::Dimensions::Two(x, y) => (x, y),
                        _ => return Err("Zigzag requires 2D geometry to be specified".to_string()),
                    };
                    Ok(Box::from(geometry::Zigzag {
                        width: w,
                        height: h,
                        major_axis: match name.chars().last().unwrap() {
                            'x' => geometry::Axis::X,
                            _ => geometry::Axis::Y,
                        },
                    }))
                }
                _ => Err(format!("Unknown transposition: {}", name)),
            }
        })
        .collect());
    Ok((0..dimensions.size())
        .map(|index| transpositions.transpose(index))
        .collect())
}

fn artnet_discover() -> io::Result<()> {
    let discovery_stream = artnet::discover();
    let mut discovered: collections::HashSet<net::SocketAddr> = collections::HashSet::new();

    let (close_tx, close_rx) = sync::mpsc::sync_channel(0);
    thread::spawn(move || {
        let mut out = io::stderr();
        for ch in ['|', '/', '-', '\\'].iter().cycle() {
            if let Ok(_) = close_rx.try_recv() {
                break;
            }
            write!(&mut out, "\r{}", ch).unwrap();
            out.flush().unwrap();
            thread::sleep(time::Duration::new(0, 100_000_000));
        }
    });

    for result in discovery_stream {
        let node = match result {
            Ok(node) => node,
            Err(err) => {
                close_tx.send(()).unwrap();
                eprint!("\r");
                return Err(err);
            }
        };
        if !discovered.contains(&node.0) {
            let ip_str = format!("{}", node.0.ip()); // Padding only works with strings. :(
            match node.1 {
                Some(name) => eprintln!("\r{: <15} -> {}", ip_str, name),
                None => eprintln!("\r{: <15}", ip_str),
            };
        }
        discovered.insert(node.0);
    }
    Ok(())
}
