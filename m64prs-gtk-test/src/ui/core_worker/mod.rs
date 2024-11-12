use std::{
    env,
    error::Error,
    fs, mem,
    path::{Path, PathBuf},
    sync::Arc,
    thread::{self, JoinHandle},
};

use m64prs_core::{plugin::PluginSet, Plugin};
use m64prs_sys::EmuState;
use relm4::{ComponentSender, Worker};

#[derive(Debug)]
pub enum Request {
    Init,
    StartRom(PathBuf),
    StopRom,
}

#[derive(Debug)]
pub enum Update {
    CoreReady,
    Error(Box<dyn Error + Send + 'static>),
    EmuStateChange(EmuState),
}

/// Inner enum representing the model's current state.
#[derive(Debug)]
enum ModelInner {
    /// The core is not initialized yet.
    Uninit,
    /// The core is ready to use. It does not have a ROM open.
    Ready(m64prs_core::Core),
    /// The core is running a ROM in a background thread.
    Running {
        join_handle: JoinHandle<()>,
        core_ref: Arc<m64prs_core::Core>,
    },
}

#[derive(Debug)]
pub struct Model(ModelInner);

impl Model {
    fn init(&mut self, sender: &ComponentSender<Self>) {
        #[cfg(target_os = "windows")]
        const MUPEN_FILENAME: &str = "mupen64plus.dll";
        #[cfg(target_os = "linux")]
        const MUPEN_FILENAME: &str = "libmupen64plus.so";

        self.0 = match self.0 {
            ModelInner::Uninit => {
                let self_path = env::current_exe().expect("should be able to find current_exe");
                let mupen_dll_path = self_path
                    .parent()
                    .map(|path| path.join(MUPEN_FILENAME))
                    .expect("should be able to access other file in the same folder");

                let core =
                    m64prs_core::Core::init(mupen_dll_path).expect("core startup should succeed");

                ModelInner::Ready(core)
            }
            _ => panic!("core is already initialized"),
        };
        sender.output(Update::CoreReady).unwrap();
    }

    fn start_rom(&mut self, path: &Path, sender: &ComponentSender<Self>) {
        self.0 = match mem::replace(&mut self.0, ModelInner::Uninit) {
            ModelInner::Uninit => panic!("core should be initialized"),
            ModelInner::Ready(core) => 'core_ready: {
                let rom_data = match fs::read(path) {
                    Ok(data) => data,
                    Err(error) => {
                        let _ = sender.output(Update::Error(Box::new(error)));
                        break 'core_ready ModelInner::Ready(core);
                    }
                };
                Self::start_rom_inner(&rom_data, core, sender)
            }
            ModelInner::Running {
                join_handle,
                core_ref,
            } => 'core_running: {
                let rom_data = match fs::read(path) {
                    Ok(data) => data,
                    Err(error) => {
                        let _ = sender.output(Update::Error(Box::new(error)));
                        break 'core_running ModelInner::Running {
                            join_handle,
                            core_ref,
                        };
                    }
                };
                let core = Self::stop_rom_inner(join_handle, core_ref, sender);
                Self::start_rom_inner(&rom_data, core, sender)
            }
        };

    }

    fn stop_rom(&mut self, sender: &ComponentSender<Self>) {
        self.0 = match mem::replace(&mut self.0, ModelInner::Uninit) {
            ModelInner::Running {
                join_handle,
                core_ref,
            } => {
                let mut core = Self::stop_rom_inner(join_handle, core_ref, sender);

                core.detach_plugins();
                core.close_rom().expect("there should be an open ROM");

                ModelInner::Ready(core)
            }
            _ => panic!("core should be running"),
        };
    }
}

/// Internal functions behind the requests.
impl Model {
    fn start_rom_inner(
        rom_data: &[u8],
        mut core: m64prs_core::Core,
        sender: &ComponentSender<Self>,
    ) -> ModelInner {
        macro_rules! check {
            ($res:expr) => {
                match ($res) {
                    Ok(value) => value,
                    Err(err) => {
                        let _ = sender.output(Update::Error(Box::new(err)));
                        return ModelInner::Ready(core);
                    }
                }
            };
        }

        let plugins = PluginSet {
            graphics: check!(Plugin::load(
                "/usr/lib/mupen64plus/mupen64plus-video-rice.so"
            )),
            audio: check!(Plugin::load(
                "/usr/lib/mupen64plus/mupen64plus-audio-sdl.so"
            )),
            input: check!(Plugin::load(
                "/usr/lib/mupen64plus/mupen64plus-input-sdl.so"
            )),
            rsp: check!(Plugin::load("/usr/lib/mupen64plus/mupen64plus-rsp-hle.so")),
        };

        check!(core.open_rom(&rom_data));

        if let Err(err) = core.attach_plugins(plugins) {
            let _ = sender.output(Update::Error(Box::new(err)));
            core.close_rom()
                .expect("there should be an open ROM");
            return ModelInner::Ready(core);
        }

        let core_ref = Arc::new(core);

        let join_handle = {
            let core = Arc::clone(&core_ref);
            thread::spawn(move || {
                let _ = core.execute();
            })
        };

        pollster::block_on(core_ref.await_emu_state(EmuState::Running));
        let _ = sender.output(Update::EmuStateChange(EmuState::Running));

        ModelInner::Running {
            join_handle,
            core_ref,
        }
    }

    fn stop_rom_inner(
        join_handle: JoinHandle<()>,
        core_ref: Arc<m64prs_core::Core>,
        sender: &ComponentSender<Self>,
    ) -> m64prs_core::Core {
        pollster::block_on(core_ref.stop()).expect("the core should be running");
        join_handle.join().expect("the core thread shouldn't panic");

        Arc::into_inner(core_ref)
            .expect("no refs to the core should exist outside of the emulator thread")
    }
}

impl Worker for Model {
    type Init = ();

    type Input = Request;
    type Output = Update;

    fn init(_: Self::Init, sender: ComponentSender<Self>) -> Self {
        let result = Self(ModelInner::Uninit);
        sender.input(Request::Init);
        result
    }

    fn update(&mut self, request: Self::Input, sender: ComponentSender<Self>) {
        match request {
            Request::Init => self.init(&sender),
            Request::StartRom(path) => self.start_rom(&path, &sender),
            Request::StopRom => self.stop_rom(&sender),
        }
    }
}
