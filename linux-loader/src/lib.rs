//! Linux LibOS
//! - run process and manage trap/interrupt/syscall
#![no_std]
#![feature(asm)]
#![feature(global_asm)]
#![deny(warnings, unused_must_use, missing_docs)]

extern crate alloc;
#[macro_use]
extern crate log;

use {
    alloc::{boxed::Box, string::String, sync::Arc, vec::Vec},
    core::{future::Future, pin::Pin},
    kernel_hal::{MMUFlags, UserContext},
    linux_object::{
        fs::{vfs::FileSystem, INodeExt},
        loader::LinuxElfLoader,
        process::ProcessExt,
        thread::ThreadExt,
    },
    linux_syscall::Syscall,
    zircon_object::task::*,
};

/// Create and run main Linux process
pub fn run(args: Vec<String>, envs: Vec<String>, rootfs: Arc<dyn FileSystem>) -> Arc<Process> {
    let job = Job::root();
    let proc = Process::create_linux(&job, rootfs.clone()).unwrap();
    let thread = Thread::create_linux(&proc).unwrap();
    let loader = LinuxElfLoader {
        #[cfg(feature = "std")]
        syscall_entry: kernel_hal_unix::syscall_entry as usize,
        #[cfg(not(feature = "std"))]
        syscall_entry: 0,
        stack_pages: 8,
        root_inode: rootfs.root_inode(),
    };
    let inode = rootfs.root_inode().lookup(&args[0]).unwrap();
    let data = inode.read_as_vec().unwrap();
    let path = args[0].clone();
    let (entry, sp) = loader.load(&proc.vmar(), &data, args, envs, path).unwrap();

    thread
        .start(entry, sp, 0, 0, thread_fn)
        .expect("failed to start main thread");
    proc
}

/// The function of a new thread.
///
/// loop:
/// - wait for the thread to be ready
/// - get user thread context
/// - enter user mode
/// - handle trap/interrupt/syscall according to the return value
/// - return the context to the user thread
async fn new_thread(thread: CurrentThread) {
    loop {
        // wait
        let mut cx = thread.wait_for_run().await;
        if thread.state() == ThreadState::Dying {
            break;
        }
        // run
        trace!("go to user: {:#x?}", cx);
        kernel_hal::context_run(&mut cx);
        trace!("back from user: {:#x?}", cx);
        // handle trap/interrupt/syscall
        #[cfg(target_arch = "x86_64")]
        match cx.trap_num {
            0x100 => handle_syscall(&thread, &mut cx.general).await,
            0x20..=0x3f => {
                kernel_hal::InterruptManager::handle(cx.trap_num as u8);
                if cx.trap_num == 0x20 {
                    kernel_hal::yield_now().await;
                }
            }
            0xe => {
                let vaddr = kernel_hal::fetch_fault_vaddr();
                let flags = if cx.error_code & 0x2 == 0 {
                    MMUFlags::READ
                } else {
                    MMUFlags::WRITE
                };
                error!("page fualt from user mode {:#x} {:#x?}", vaddr, flags);
                let vmar = thread.proc().vmar();
                match vmar.handle_page_fault(vaddr, flags) {
                    Ok(()) => {}
                    Err(_) => {
                        panic!("Page Fault from user mode {:#x?}", cx);
                    }
                }
            }
            _ => panic!("not supported interrupt from user mode. {:#x?}", cx),
        }
        #[cfg(target_arch = "riscv64")]
        let trap_num = riscv::register::scause::read().bits();
        #[cfg(target_arch = "riscv64")]
        match trap_num {
            // page fault
            _ if kernel_hal_bare::arch::is_page_fault(trap_num) => {
                let addr = kernel_hal_bare::arch::get_page_fault_addr();
                info!("page fault from user @ {:#x}", addr);
                let flags = if trap_num == 15 {
                    MMUFlags::WRITE
                } else {
                    MMUFlags::READ
                };
                let vmar = thread.proc().vmar();
                match vmar.handle_page_fault(addr, flags) {
                    Ok(()) => {}
                    Err(err) => {
                        panic!("Page Fault from user mode {:#x?}, tf = {:#x?}", err, cx);
                    }
                }
            }
            _ if kernel_hal_bare::arch::is_timer_intr(trap_num) => {
                kernel_hal_bare::arch::clock_set_next_event();
                kernel_hal::timer_tick();
            }
            _ if kernel_hal_bare::arch::is_syscall(trap_num) => handle_syscall(&thread, &mut cx).await,
            _ => panic!("unhandled trap {:?}", riscv::register::scause::read().cause()),
        }
        thread.end_running(cx);
    }
}

fn thread_fn(thread: CurrentThread) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
    Box::pin(new_thread(thread))
}

/// syscall handler entry
async fn handle_syscall(thread: &CurrentThread, context: &mut UserContext) {
    trace!("syscall: {:#x?}", context);
    let num = context.get_syscall_num();
    let args = context.get_syscall_args();
    let regs = &mut context.general;
    #[cfg(target_arch = "riscv64")]
    {
        context.sepc = context.sepc + 4;
    }
    let mut syscall = Syscall {
        thread,
        #[cfg(feature = "std")]
        syscall_entry: kernel_hal_unix::syscall_entry as usize,
        #[cfg(not(feature = "std"))]
        syscall_entry: 0,
        thread_fn,
        regs,
    };
    let ret = syscall.syscall(num as u32, args).await;
    context.set_syscall_ret(ret as usize);
}
