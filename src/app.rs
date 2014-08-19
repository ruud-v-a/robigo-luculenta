// Robigo Luculenta -- Proof of concept spectral path tracer in Rust
// Copyright (C) 2014 Ruud van Asseldonk
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::comm::{Handle, Select, Sender, Receiver, channel};
use std::io::timer::sleep;
use std::os::num_cpus;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::vec::unzip;
use camera::Camera;
use gather_unit::GatherUnit;
use geometry::{Plane, Sphere};
use material::{BlackBodyMaterial, DiffuseColouredMaterial};
use object::{Emissive, Object, Reflective};
use plot_unit::PlotUnit;
use quaternion::Quaternion;
use scene::Scene;
use task_scheduler::{Task, Sleep, Trace, Plot, Gather, Tonemap, TaskScheduler};
use tonemap_unit::TonemapUnit;
use trace_unit::TraceUnit;
use vector3::Vector3;

pub type Image = Vec<u8>;

/// Width of the canvas.
pub static image_width: uint = 1280;

/// Height of the canvas.
pub static image_height: uint = 720;

/// Canvas aspect ratio.
static aspect_ratio: f32 = image_width as f32 / image_height as f32;

pub struct App {
    /// Channel that can be used to signal the application to stop.
    pub stop: Sender<()>,

    /// Channel that produces a rendered image periodically.
    pub images: Receiver<Image>
}

impl App {
    pub fn new() -> App {
        let concurrency = num_cpus();
        let ts = TaskScheduler::new(concurrency, image_width, image_height);
        let task_scheduler = Arc::new(Mutex::new(ts));

        // Channels for communicating back to the main task.
        let (stop_tx, stop_rx) = channel::<()>();
        let (img_tx, img_rx) = channel();

        // Then spawn a supervisor task that will start the workers.
        spawn(proc() {
            // Spawn as many workers as cores.
            let (stop_workers, images) = unzip(
            range(0u, concurrency)
            .map(|_| { App::start_worker(task_scheduler.clone()) }));
            
            // Combine values so we can recv one at a time.
            let select = Select::new();
            let mut worker_handles: Vec<Handle<Image>> = images
            .iter().map(|worker_rx| {
                let mut handle = select.handle(worker_rx);
                unsafe { handle.add(); }
                handle
            }).collect();
            let mut stop_handle = select.handle(&stop_rx);
            unsafe { stop_handle.add(); }
            
            // Then go into the supervising loop: broadcast a stop signal to
            // all workers, or route a rendered image to the main task.
            loop {
                let id = select.wait();

                // Was the source a worker?
                for handle in worker_handles.mut_iter() {
                    // When a new image arrives, route it to the main task.
                    if id == handle.id() {
                        let img = handle.recv();
                        img_tx.send(img);
                    }
                }

                // Or the stop channel perhaps?
                if id == stop_handle.id() {
                    // Broadcast to all workers that they should stop.
                    for stop in stop_workers.iter() {
                        stop.send(());
                    }
                    // Then also stop the supervising loop.
                    break;
                }
            }
        });

        App {
            stop: stop_tx,
            images: img_rx
        }
    }

    fn start_worker(task_scheduler: Arc<Mutex<TaskScheduler>>)
                    -> (Sender<()>, Receiver<Image>) {
        let (stop_tx, stop_rx) = channel::<()>();
        let (img_tx, img_rx) = channel::<Image>();

        spawn(proc() {
            // TODO: there should be one scene for the entire program,
            // not one per worker thread. However, I can't get sharing
            // the scene working properly :(
            let scene = App::set_up_scene();

            // Move img_tx into the proc.
            let mut owned_img_tx = img_tx;

            // There is no task yet, but the task scheduler expects
            // a completed task. Therefore, this worker is done sleeping.
            let mut task = Sleep;

            // Until something signals this worker to stop,
            // continue executing tasks.
            loop {
                // Ask the task scheduler for a new task, complete the old one.
                // Then execute it.
                task = task_scheduler.lock().get_new_task(task);
                App::execute_task(&mut task, &scene, &mut owned_img_tx);

                // Stop only if a stop signal has been sent.
                match stop_rx.try_recv() {
                    Ok(()) => break,
                    _ => { }
                }
            }
        });

        // TODO: spawn proc.
        (stop_tx, img_rx)
    }

    fn execute_task(task: &mut Task, scene: &Scene, img_tx: &mut Sender<Image>) {
        match *task {
            Sleep =>
                App::execute_sleep_task(),
            Trace(ref mut trace_unit) =>
                App::execute_trace_task(scene, &mut **trace_unit),
            Plot(ref mut plot_unit, ref mut units) =>
                App::execute_plot_task(&mut **plot_unit, units.as_mut_slice()),
            Gather(ref mut gather_unit, ref mut units) =>
                App::execute_gather_task(&mut **gather_unit, units.as_mut_slice()),
            Tonemap(ref mut tonemap_unit, ref mut gather_unit) =>
                App::execute_tonemap_task(img_tx, &mut **tonemap_unit, &mut **gather_unit)
        }
    }

    fn execute_sleep_task() {
        sleep(Duration::milliseconds(100));
    }

    fn execute_trace_task(scene: &Scene, trace_unit: &mut TraceUnit) {
        trace_unit.render(scene);
    }

    fn execute_plot_task(plot_unit: &mut PlotUnit,
                         units: &mut[Box<TraceUnit>]) {
        for unit in units.mut_iter() {
            plot_unit.plot(unit.mapped_photons);
        }
    }

    fn execute_gather_task(gather_unit: &mut GatherUnit,
                           units: &mut[Box<PlotUnit>]) {
        for unit in units.mut_iter() {
            gather_unit.accumulate(unit.tristimulus_buffer.as_slice());
            unit.clear();
        }
    }

    fn execute_tonemap_task(img_tx: &mut Sender<Image>,
                            tonemap_unit: &mut TonemapUnit,
                            gather_unit: &mut GatherUnit) {
        tonemap_unit.tonemap(gather_unit.tristimulus_buffer.as_slice());

        // Copy the rendered image.
        let img = tonemap_unit.rgb_buffer.clone();

        // And send it to the UI / main task.
        img_tx.send(img);
    }

    fn set_up_scene() -> Scene {
        fn make_camera(_: f32) -> Camera {
            Camera {
                position: Vector3::new(0.0, 1.0, -10.0),
                field_of_view: Float::frac_pi_2(),
                focal_distance: 10.0,
                depth_of_field: 1.0,
                chromatic_abberation: 0.1,
                orientation: Quaternion::rotation(1.0, 0.0, 0.0, 1.531)
            }
        }

        let red = DiffuseColouredMaterial::new(0.9, 700.0, 120.0);
        let plane = Plane::new(Vector3::new(0.0, 1.0, 0.0), Vector3::zero());
        let sphere = Sphere::new(Vector3::zero(), 2.0);
        let black_body = BlackBodyMaterial::new(6504.0, 1.0);
        let reflective = Object::new(box plane, Reflective(box red));
        let emissive = Object::new(box sphere, Emissive(box black_body));
        Scene {
            objects: vec!(reflective, emissive),
            get_camera_at_time: make_camera
        }
    }
}