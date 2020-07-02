/*
 *   This file is part of NCC Group Scrying https://github.com/nccgroup/scrying
 *   Copyright 2020 David Young <david(dot)young(at)nccgroup(dot)com>
 *   Released as open source by NCC Group Plc - https://www.nccgroup.com
 *
 *   Scrying is free software: you can redistribute it and/or modify
 *   it under the terms of the GNU General Public License as published by
 *   the Free Software Foundation, either version 3 of the License, or
 *   (at your option) any later version.
 *
 *   Scrying is distributed in the hope that it will be useful,
 *   but WITHOUT ANY WARRANTY; without even the implied warranty of
 *   MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 *   GNU General Public License for more details.
 *
 *   You should have received a copy of the GNU General Public License
 *   along with Scrying.  If not, see <https://www.gnu.org/licenses/>.
*/

use crate::argparse::Mode::Rdp;
use crate::argparse::Opts;
use crate::error::Error;
use crate::parsing::Target;
use crate::reporting::ReportMessageContent;
use crate::reporting::{FileError, ReportMessage};
use crate::util::target_to_filename;
use crate::ThreadStatus;
use image::{DynamicImage, ImageBuffer, Rgba};
#[allow(unused)]
use log::{debug, error, info, trace, warn};
use rdp::core::client::Connector;
use rdp::core::client::RdpClient;
use rdp::core::event::RdpEvent;
use socks::Socks5Stream;
use std::io::Read;
use std::io::Write;
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, mpsc::Receiver, mpsc::Sender};
use std::thread;
use std::time::Duration;

//TODO maybe make this configurable
const IMAGE_WIDTH: u16 = 1280;
const IMAGE_HEIGHT: u16 = 1024;

struct BitmapChunk {
    width: u32,
    height: u32,
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
    bpp: u32,
    data: Vec<u8>,
}

enum ImageMode {
    //HighColor16(DynamicImage),
    Rgba32(DynamicImage),
}

impl ImageMode {
    fn extract(self) -> DynamicImage {
        use ImageMode::*;
        match self {
            //HighColor16(di) => di,
            Rgba32(di) => di,
        }
    }
}

#[derive(Default)]
struct Image {
    image: Option<ImageMode>,
    //colour: Option<ColourMode>,
    component_width: Option<usize>,
    width: Option<u32>,
    height: Option<u32>,
}

impl Image {
    fn add_chunk(&mut self, chunk: &BitmapChunk) -> Result<(), ()> {
        use ImageMode::*;
        //TODO return sensible errors when things are inconsistent

        if self.image.is_none() {
            // Image type has not been determined yet
            self.initialise_buffer(chunk)?;
        }

        //TODO assert that the buffer is the right length etc.

        // If the chunk has zero size then we have a problem
        if chunk.left == chunk.right || chunk.top == chunk.bottom {
            warn!("Received zero-size chunk");
            return Err(());
        }

        let mut x: u32 = chunk.left;
        let mut y: u32 = chunk.top;

        // the enumerate is sometimes running more times that fits into
        // the height of the image
        for (idx, pixel) in
            chunk.data.chunks(self.component_width.unwrap()).enumerate()
        {
            trace!("idx: {}, pixel: {:?}, at ({}, {})", idx, pixel, x, y);

            if y > chunk.bottom {
                warn!("Pixel out of bounds!");
                break;
            }

            match &mut self.image {
                Some(Rgba32(DynamicImage::ImageRgba8(img))) => {
                    //let x: usize = img;
                    img.put_pixel(
                        x,
                        y,
                        Rgba([
                            pixel[2], pixel[1], pixel[0],
                            0xff,
                            //TODO: alpha pixel[3],
                            // Sometimes pixel[3] is correct, sometimes
                            // 0xff - pixel[3] is correct.
                        ]),
                    );
                }
                /*Some(HighColor16(DynamicImage::ImageRgb8(img))) => {
                    img.put_pixel(x, y, Rgb([pixel[0], pixel[1], 0]))
                }*/
                _ => unimplemented!(),
            }

            // Increment x and y around the chunk
            x += 1;
            if x > chunk.right {
                trace!("CR");
                x = chunk.left;
                y += 1;
            }
        }

        Ok(())
    }

    fn initialise_buffer(&mut self, chunk: &BitmapChunk) -> Result<(), ()> {
        use ImageMode::*;
        println!("BITS PER PIXEL: {}", chunk.bpp);
        //TODO get these values properly
        // IMAGE_WIDTH and IMAGE_HEIGHT are u16
        let width = IMAGE_WIDTH as u32;
        let height = IMAGE_HEIGHT as u32;

        let pixel_size = 4; //chunk.data.len() as u32
                            // / ((chunk.right - chunk.left) * (chunk.bottom - chunk.top));
        println!("PIXEL SIZE {}", pixel_size);

        // Have to do a let binding here and then transfer to the self.*
        // variables pending https://github.com/rust-lang/rfcs/pull/2909
        let (component_width, image) = match pixel_size {
            /*2 => {
                debug!("Detected HighColor16");
                (
                    // 16-bit RGB using 5 bits per colour; store as 8 bit colour
                    Some(4),
                    Some(HighColor16(DynamicImage::ImageRgb8(
                        ImageBuffer::<Rgb<u8>, Vec<u8>>::new(width, height),
                    ))),
                )
            }*/
            4 => {
                debug!("Detected RGBA-32");
                (
                    Some(4),
                    Some(Rgba32(DynamicImage::ImageRgba8(ImageBuffer::<
                        Rgba<u8>,
                        Vec<u8>,
                    >::new(
                        width, height
                    )))),
                )
            }
            _ => unimplemented!(),
        };
        self.component_width = component_width;
        self.image = image;
        self.width = Some(width);
        self.height = Some(height);

        Ok(())
    }
}

/// Wrapper enum to hold TCP and Socks5 streams. This enum implements
/// Read and Write transitively
enum SocketType {
    Socks5(Socks5Stream),
    Tcp(TcpStream),
}

impl Read for SocketType {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        use SocketType::*;
        match self {
            Socks5(s) => s.read(buf),
            Tcp(s) => s.read(buf),
        }
    }
}

impl Write for SocketType {
    fn write(
        &mut self,
        buf: &[u8],
    ) -> std::result::Result<usize, std::io::Error> {
        use SocketType::*;
        match self {
            Socks5(s) => s.write(buf),
            Tcp(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> Result<(), std::io::Error> {
        use SocketType::*;
        match self {
            Socks5(s) => s.flush(),
            Tcp(s) => s.flush(),
        }
    }
}

fn capture_worker(
    target: &Target,
    opts: &Opts,
    report_tx: &mpsc::Sender<ReportMessage>,
) -> Result<(), Error> {
    info!("Connecting to {:?}", target);
    let addr = match target {
        Target::Address(sock_addr) => sock_addr,
        Target::Url(_) => {
            return Err(Error::RdpError(format!(
                "Invalid RDP target: {}",
                target
            )));
        }
    };

    // If the proxy configuration is selected then create a Socks5
    // connection, otherwise create a regular TCP stream. The wrapper
    // enum is used to get around type errors and the limitation that
    // trait objects can only have one main trait (i.e. "dyn Read +
    // Write") is not possible.
    let stream = if let Some(proxy) = &opts.rdp_proxy {
        debug!("Connecting to Socks proxy");
        SocketType::Socks5(Socks5Stream::connect(proxy, *addr)?)
    } else {
        SocketType::Tcp(TcpStream::connect(&addr)?)
    };

    let mut connector = Connector::new()
        .screen(IMAGE_WIDTH, IMAGE_HEIGHT)
        .use_nla(false)
        .check_certificate(false)
        .blank_creds(true)
        .credentials("".to_string(), "".to_string(), "".to_string());
    let client = connector.connect(stream)?;

    let mut rdp_image: Image = Default::default();
    {
        // Spawn a thread to listen for bitmap events
        let (bmp_sender, bmp_receiver): (Sender<BitmapChunk>, Receiver<_>) =
            mpsc::channel();
        let _bmp_thread_handle = thread::spawn(move || {
            bmp_thread(client, bmp_sender);
        });

        let timeout = Duration::from_secs(2);
        loop {
            match bmp_receiver.recv_timeout(timeout) {
                Err(_) => {
                    warn!("Timeout reached");
                    break;
                }
                Ok(chunk) => {
                    if rdp_image.add_chunk(&chunk).is_err() {
                        warn!("Attempted to add invalid chunk");
                        //break;
                    }
                }
            }
        }
    }
    match rdp_image.image {
        Some(di) => {
            info!("Successfully received image");
            let filename = format!("{}.png", target_to_filename(&target));
            let relative_filepath = Path::new("rdp").join(&filename);
            let filepath = Path::new(&opts.output_dir).join(&relative_filepath);
            info!("Saving image as {}", filepath.display());
            di.extract().save(&filepath)?;
            let report_message = ReportMessage::Output(ReportMessageContent {
                mode: Rdp,
                target: target.to_string(),
                output: FileError::File(
                    relative_filepath.display().to_string(),
                ),
            });
            report_tx.send(report_message)?;
        }
        None => {
            warn!(
            "Error receiving image from {}. Perhaps the server disconnected",
            addr
            );
            return Err(Error::RdpError(format!(
                "Error receiving image, perhaps the server disconnected",
            )));
        }
    }

    Ok(())
}

fn bmp_thread<T: Read + Write>(
    mut client: RdpClient<T>,
    sender: Sender<BitmapChunk>,
) {
    let break_cond = AtomicBool::new(false);
    while !break_cond.load(Ordering::Relaxed) {
        match client.read(|rdp_event| match rdp_event {
            RdpEvent::Bitmap(bitmap) => {
                // numbers all come in as u16
                let mut chunk = BitmapChunk {
                    width: bitmap.width as u32,
                    height: bitmap.height as u32,
                    left: bitmap.dest_left as u32,
                    top: bitmap.dest_top as u32,
                    right: bitmap.dest_right as u32,
                    bottom: bitmap.dest_bottom as u32,
                    bpp: bitmap.bpp as u32,
                    data: Vec::new(),
                };

                let data = if bitmap.is_compress {
                    bitmap
                        .decompress()
                        .expect("Error decompressing bitmap chunk")
                } else {
                    bitmap.data
                };
                chunk.data = data;

                debug!(
                    "Received {}x{} bmp pos {}, {}, {}, {}, bpp: {}, len {}",
                    chunk.width,
                    chunk.height,
                    chunk.left,
                    chunk.top,
                    chunk.right,
                    chunk.bottom,
                    chunk.bpp,
                    chunk.data.len(),
                );

                if sender.send(chunk).is_err() {
                    // Recevier disconnected, most likely because the timeout
                    // was reached
                    info!("Bitmap channel disconnected");
                    break_cond.store(true, Ordering::Relaxed);
                }
            }
            RdpEvent::Pointer(_) => info!("Pointer event!"),
            RdpEvent::Key(_) => info!("Key event!"),
        }) {
            Ok(_) => (),
            Err(e) => {
                error!("Error reading RDP stream: {:?}", e);
                return;
            }
        }
    }
}

pub fn capture(
    target: &Target,
    opts: &Opts,
    tx: mpsc::Sender<ThreadStatus>,
    report_tx: &mpsc::Sender<ReportMessage>,
) {
    if let Err(e) = capture_worker(target, opts, report_tx) {
        warn!("error: {}", e);
        let report_message = ReportMessage::Output(ReportMessageContent {
            mode: Rdp,
            target: target.to_string(),
            output: FileError::Error(e.to_string()),
        });
        report_tx
            .send(report_message)
            .expect("Reporting thread seems to have disconnected");
    }

    tx.send(ThreadStatus::Complete).unwrap();
}
