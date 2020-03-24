//
// Sysinfo
//
// Copyright (c) 2018 Guillaume Gomez
//

use sys::component::{self, Component};
use sys::disk::Disk;
use sys::processor::*;
use sys::users::get_users;

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::time::SystemTime;

use LoadAvg;
use Networks;
use Pid;
use ProcessExt;
use RefreshKind;
use SystemExt;
use User;

use windows::process::{
    compute_cpu_usage, get_handle, get_system_computation_time, update_proc_info, Process,
};
use windows::tools::*;

use winapi::shared::minwindef::FALSE;
use winapi::um::minwinbase::STILL_ACTIVE;
use winapi::um::processthreadsapi::GetExitCodeProcess;
use winapi::um::sysinfoapi::{GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX};
use winapi::um::winnt::HANDLE;

use rayon::prelude::*;

/// Struct containing the system's information.
pub struct System {
    process_list: HashMap<usize, Process>,
    mem_total: u64,
    mem_free: u64,
    swap_total: u64,
    swap_free: u64,
    global_processor: Processor,
    processors: Vec<Processor>,
    components: Vec<Component>,
    disks: Vec<Disk>,
    query: Option<Query>,
    networks: Networks,
    boot_time: u64,
    users: Vec<User>,
}

// Useful for parallel iterations.
struct Wrap<T>(T);

unsafe impl<T> Send for Wrap<T> {}
unsafe impl<T> Sync for Wrap<T> {}

unsafe fn boot_time() -> u64 {
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(n) => n.as_secs() - GetTickCount64() / 1000,
        Err(_e) => {
            #[cfg(feature = "debug")]
            {
                println!("Failed to compute boot time: {:?}", _e);
            }
            0
        }
    }
}

impl SystemExt for System {
    #[allow(non_snake_case)]
    fn new_with_specifics(refreshes: RefreshKind) -> System {
        let (processors, vendor_id, brand) = init_processors();
        let mut s = System {
            process_list: HashMap::with_capacity(500),
            mem_total: 0,
            mem_free: 0,
            swap_total: 0,
            swap_free: 0,
            global_processor: Processor::new_with_values("Total CPU", vendor_id, brand, 0),
            processors,
            components: Vec::new(),
            disks: Vec::with_capacity(2),
            query: Query::new(),
            networks: Networks::new(),
            boot_time: unsafe { boot_time() },
            users: Vec::new(),
        };
        // TODO: in case a translation fails, it might be nice to log it somewhere...
        if let Some(ref mut query) = s.query {
            let x = unsafe { load_symbols() };
            if let Some(processor_trans) = get_translation(&"Processor".to_owned(), &x) {
                // let idle_time_trans = get_translation(&"% Idle Time".to_owned(), &x);
                let proc_time_trans = get_translation(&"% Processor Time".to_owned(), &x);
                if let Some(ref proc_time_trans) = proc_time_trans {
                    add_counter(
                        format!("\\{}(_Total)\\{}", processor_trans, proc_time_trans),
                        query,
                        get_key_used(&mut s.global_processor),
                        "tot_0".to_owned(),
                    );
                }
                for (pos, proc_) in s.processors.iter_mut().enumerate() {
                    if let Some(ref proc_time_trans) = proc_time_trans {
                        add_counter(
                            format!("\\{}({})\\{}", processor_trans, pos, proc_time_trans),
                            query,
                            get_key_used(proc_),
                            format!("{}_0", pos),
                        );
                    }
                }
            } else {
                eprintln!("failed to get `Processor` translation");
            }
        }
        s.refresh_specifics(refreshes);
        s
    }

    fn refresh_cpu(&mut self) {
        if let Some(ref mut query) = self.query {
            query.refresh();
            let mut used_time = None;
            if let &mut Some(ref key_used) = get_key_used(&mut self.global_processor) {
                used_time = Some(
                    query
                        .get(&key_used.unique_id)
                        .expect("global_key_idle disappeared"),
                );
            }
            if let Some(used_time) = used_time {
                self.global_processor.set_cpu_usage(used_time);
            }
            for p in self.processors.iter_mut() {
                let mut used_time = None;
                if let &mut Some(ref key_used) = get_key_used(p) {
                    used_time = Some(
                        query
                            .get(&key_used.unique_id)
                            .expect("key_used disappeared"),
                    );
                }
                if let Some(used_time) = used_time {
                    p.set_cpu_usage(used_time);
                }
            }
        }
    }

    fn refresh_memory(&mut self) {
        unsafe {
            let mut mem_info: MEMORYSTATUSEX = zeroed();
            mem_info.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
            GlobalMemoryStatusEx(&mut mem_info);
            self.mem_total = auto_cast!(mem_info.ullTotalPhys, u64);
            self.mem_free = auto_cast!(mem_info.ullAvailPhys, u64);
            //self.swap_total = auto_cast!(mem_info.ullTotalPageFile - mem_info.ullTotalPhys, u64);
            //self.swap_free = auto_cast!(mem_info.ullAvailPageFile, u64);
        }
    }

    fn refresh_components_list(&mut self) {
        self.components = component::get_components();
    }

    fn refresh_process(&mut self, pid: Pid) -> bool {
        if self.process_list.contains_key(&pid) {
            if refresh_existing_process(self, pid, true) == false {
                self.process_list.remove(&pid);
                return false;
            }
            true
        } else if let Some(mut p) = Process::new_from_pid(pid) {
            let system_time = get_system_computation_time();
            compute_cpu_usage(&mut p, self.processors.len() as u64, system_time);
            self.process_list.insert(pid, p);
            true
        } else {
            false
        }
    }

    #[allow(clippy::cast_ptr_alignment)]
    fn refresh_processes(&mut self) {}

    fn refresh_disks_list(&mut self) {
        self.disks = unsafe { get_disks() };
    }

    fn refresh_users_list(&mut self) {
        self.users = unsafe { get_users() };
    }

    fn get_processes(&self) -> &HashMap<Pid, Process> {
        &self.process_list
    }

    fn get_process(&self, pid: Pid) -> Option<&Process> {
        self.process_list.get(&(pid as usize))
    }

    fn get_global_processor_info(&self) -> &Processor {
        &self.global_processor
    }

    fn get_processors(&self) -> &[Processor] {
        &self.processors
    }

    fn get_total_memory(&self) -> u64 {
        self.mem_total >> 10
    }

    fn get_free_memory(&self) -> u64 {
        self.mem_free >> 10
    }

    fn get_used_memory(&self) -> u64 {
        (self.mem_total - self.mem_free) >> 10
    }

    fn get_total_swap(&self) -> u64 {
        self.swap_total >> 10
    }

    fn get_free_swap(&self) -> u64 {
        self.swap_free >> 10
    }

    fn get_used_swap(&self) -> u64 {
        (self.swap_total - self.swap_free) >> 10
    }

    fn get_components(&self) -> &[Component] {
        &self.components
    }

    fn get_components_mut(&mut self) -> &mut [Component] {
        &mut self.components
    }

    fn get_disks(&self) -> &[Disk] {
        &self.disks
    }

    fn get_disks_mut(&mut self) -> &mut [Disk] {
        &mut self.disks
    }

    fn get_users(&self) -> &[User] {
        &self.users
    }

    fn get_networks(&self) -> &Networks {
        &self.networks
    }

    fn get_networks_mut(&mut self) -> &mut Networks {
        &mut self.networks
    }

    fn get_uptime(&self) -> u64 {
        unsafe { GetTickCount64() / 1000 }
    }

    fn get_boot_time(&self) -> u64 {
        self.boot_time
    }

    fn get_load_average(&self) -> LoadAvg {
        get_load_average()
    }
}

impl Default for System {
    fn default() -> System {
        System::new()
    }
}

fn is_proc_running(handle: HANDLE) -> bool {
    let mut exit_code = 0;
    let ret = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    !(ret == FALSE || exit_code != STILL_ACTIVE)
}

fn refresh_existing_process(s: &mut System, pid: Pid, compute_cpu: bool) -> bool {
    if let Some(ref mut entry) = s.process_list.get_mut(&(pid as usize)) {
        if !is_proc_running(get_handle(entry)) {
            return false;
        }
        update_proc_info(entry);
        if compute_cpu {
            compute_cpu_usage(
                entry,
                s.processors.len() as u64,
                get_system_computation_time(),
            );
        }
        true
    } else {
        false
    }
}
