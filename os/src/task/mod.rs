//! Task management implementation
//!
//! Everything about task management, like starting and switching tasks is
//! implemented here.
//!
//! A single global instance of [`TaskManager`] called `TASK_MANAGER` controls
//! all the tasks in the operating system.
//!
//! Be careful when you see `__switch` ASM function in `switch.S`. Control flow around this function
//! might not be what you expect.

mod context;
mod switch;
#[allow(clippy::module_inception)]
mod task;

use crate::loader::{get_app_data, get_num_app};
use crate::sync::UPSafeCell;
use crate::timer::get_time_ms;
use crate::trap::TrapContext;
use crate::config::MAX_SYSCALL_NUM;
use alloc::vec::Vec;
pub use crate::syscall::process::TaskInfo;
use lazy_static::*;
use switch::__switch;
pub use crate::mm::page_table::PageTable;
pub use crate::mm::address::{PhysAddr, VirtAddr};
pub use crate::mm::*;
pub use task::{TaskControlBlock, TaskStatus};

pub use context::TaskContext;

/// The task manager, where all the tasks are managed.
///
/// Functions implemented on `TaskManager` deals with all task state transitions
/// and task context switching. For convenience, you can find wrappers around it
/// in the module level.
///
/// Most of `TaskManager` are hidden behind the field `inner`, to defer
/// borrowing checks to runtime. You can see examples on how to use `inner` in
/// existing functions on `TaskManager`.
pub struct TaskManager {
    /// total number of tasks
    num_app: usize,
    /// use inner value to get mutable access
    inner: UPSafeCell<TaskManagerInner>,
}

/// The task manager inner in 'UPSafeCell'
struct TaskManagerInner {
    /// task list
    tasks: Vec<TaskControlBlock>,
    /// id of current `Running` task
    current_task: usize,
}

lazy_static! {
    /// a `TaskManager` global instance through lazy_static!
    pub static ref TASK_MANAGER: TaskManager = {
        println!("init TASK_MANAGER");
        let num_app = get_num_app();
        println!("num_app = {}", num_app);
        let mut tasks: Vec<TaskControlBlock> = Vec::new();
        for i in 0..num_app {
            tasks.push(TaskControlBlock::new(get_app_data(i), i));
        }
        TaskManager {
            num_app,
            inner: unsafe {
                UPSafeCell::new(TaskManagerInner {
                    tasks,
                    current_task: 0,
                })
            },
        }
    };
}

impl TaskManager {
    /// Run the first task in task list.
    ///
    /// Generally, the first task in task list is an idle task (we call it zero process later).
    /// But in ch4, we load apps statically, so the first task is a real app.
    fn run_first_task(&self) -> ! {
        let mut inner = self.inner.exclusive_access();
        let next_task = &mut inner.tasks[0];
        next_task.task_status = TaskStatus::Running;
        let next_task_cx_ptr = &next_task.task_cx as *const TaskContext;
        drop(inner);
        let mut _unused = TaskContext::zero_init();
        // before this, we should drop local variables that must be dropped manually
        unsafe {
            __switch(&mut _unused as *mut _, next_task_cx_ptr);
        }
        panic!("unreachable in run_first_task!");
    }

    /// Change the status of current `Running` task into `Ready`.
    fn mark_current_suspended(&self) {
        let mut inner = self.inner.exclusive_access();
        let cur = inner.current_task;
        inner.tasks[cur].task_status = TaskStatus::Ready;
    }

    /// Change the status of current `Running` task into `Exited`.
    fn mark_current_exited(&self) {
        let mut inner = self.inner.exclusive_access();
        let cur = inner.current_task;
        inner.tasks[cur].task_status = TaskStatus::Exited;
    }

    /// Find next task to run and return task id.
    ///
    /// In this case, we only return the first `Ready` task in task list.
    fn find_next_task(&self) -> Option<usize> {
        let inner = self.inner.exclusive_access();
        let current = inner.current_task;
        (current + 1..current + self.num_app + 1)
            .map(|id| id % self.num_app)
            .find(|id| inner.tasks[*id].task_status == TaskStatus::Ready)
    }

    /// Get the current 'Running' task's token.
    fn get_current_token(&self) -> usize {
        let inner = self.inner.exclusive_access();
        inner.tasks[inner.current_task].get_user_token()
    }

    /// Get the current 'Running' task's trap contexts.
    fn get_current_trap_cx(&self) -> &'static mut TrapContext {
        let inner = self.inner.exclusive_access();
        inner.tasks[inner.current_task].get_trap_cx()
    }

    /// Change the current 'Running' task's program break
    pub fn change_current_program_brk(&self, size: i32) -> Option<usize> {
        let mut inner = self.inner.exclusive_access();
        let cur = inner.current_task;
        inner.tasks[cur].change_program_brk(size)
    }

    /// Switch current `Running` task to the task we have found,
    /// or there is no `Ready` task and we can exit with all applications completed
    fn run_next_task(&self) {
        if let Some(next) = self.find_next_task() {
            let mut inner = self.inner.exclusive_access();
            let current = inner.current_task;
            inner.tasks[next].task_status = TaskStatus::Running;
            inner.current_task = next;
            let current_task_cx_ptr = &mut inner.tasks[current].task_cx as *mut TaskContext;
            let next_task_cx_ptr = &inner.tasks[next].task_cx as *const TaskContext;
            drop(inner);
            // before this, we should drop local variables that must be dropped manually
            unsafe {
                __switch(current_task_cx_ptr, next_task_cx_ptr);
            }
            // go back to user mode
        } else {
            panic!("All applications completed!");
        }
    }
    /// get pa from va
    pub fn get_pa_from_va(&self, va: usize) -> usize {
        let inner = self.inner.exclusive_access();
        let page_table = PageTable::from_token(inner.tasks[inner.current_task].get_user_token());
        let _va = VirtAddr::from(va);
        let Some(pa) = page_table.find_pte(_va.clone().floor()).map(|pte| {
            //println!("translate_va:va = {:?}", va);
            let aligned_pa: PhysAddr = pte.ppn().into();
            //println!("translate_va:pa_align = {:?}", aligned_pa);
            let offset = _va.page_offset();
            let aligned_pa_usize: usize = aligned_pa.into();
            (aligned_pa_usize + offset).into()
        }) else {
            panic!("Failed to get physical address from virtual address");
        };
        pa
    }

    /// schedule mark
    pub fn schedule_mark(&self) {
        let mut inner = self.inner.exclusive_access();
        let curr_id = inner.current_task;
        let current_task = &mut inner.tasks[curr_id];
        if  !current_task.is_scheduled {
            current_task.is_scheduled = true;
            current_task.first_scheduled_time = get_time_ms();
        }
    }

    
    ///Todo
    pub fn get_current_status(&self) -> TaskStatus {
        let inner = self.inner.exclusive_access();
        inner.tasks[inner.current_task].task_status
    }
    /// TODO
    pub fn record_this_call(&self, syscall_id: usize) {
        let mut inner = self.inner.exclusive_access();
        let curr_task_id = inner.current_task;
        inner.tasks[curr_task_id].syscall_times[syscall_id] += 1;
    }
    /// 获取当前任务的系统调用次数
    pub fn get_currtask_syscall_time(&self) -> [u32; MAX_SYSCALL_NUM] {
        let inner = self.inner.exclusive_access();
        inner.tasks[inner.current_task].syscall_times
    }
    /// TODO
    pub fn get_currtask_first_scheduled_time(&self) -> usize {
        let inner = self.inner.exclusive_access();
        inner.tasks[inner.current_task].first_scheduled_time
    }

    fn mmap(&self, start: usize, len: usize, prot: usize)->isize{
        if (prot & 0x7 == 0) || (prot & !0x7 != 0) {
            return -1
        }
        let mut right = MapPermission::U;
        if prot & 0x1 == 0x1 {right = right | MapPermission::R;}
        if prot & 0x2 == 0x2 {right = right | MapPermission::W;}
        if prot & 0x4 == 0x4 {right = right | MapPermission::X;}
        let mut inner = self.inner.exclusive_access();
        let current_task = inner.current_task;
        let memory_set = &mut (inner.tasks[current_task].memory_set);
        let start_va = VirtAddr::from(start);
        let end_va = VirtAddr::from(start+len);
        
        if memory_set.check_used(start_va, end_va) {
            return -1;
        } 
        if start_va.0 & 0xfff != 0{
            return -1;
        }
        memory_set.insert_framed_area(start_va, 
            end_va, right);
        0
    }

    fn munmap(&self, start: usize, len: usize)->isize{
        let mut inner = self.inner.exclusive_access();
        let current_task = inner.current_task;
        let memory_set = &mut (inner.tasks[current_task].memory_set);
        let start_va = VirtAddr::from(start);
        let end_va = VirtAddr::from(start+len);
        if memory_set.check_unused(start_va, end_va) {
            return -1;
        }
        if start_va.0 & 0xfff != 0{
            return -1;
        }
        memory_set.delete_area(start_va, end_va);
        0
    }

}

/// get the physical address from the virtual address
pub fn get_physical_addr(va: usize) -> usize {
    TASK_MANAGER.get_pa_from_va(va)
}
/// Run the first task in task list.
pub fn run_first_task() {
    TASK_MANAGER.run_first_task();
}

/// Switch current `Running` task to the task we have found,
/// or there is no `Ready` task and we can exit with all applications completed
fn run_next_task() {
    TASK_MANAGER.run_next_task();
}

/// Change the status of current `Running` task into `Ready`.
fn mark_current_suspended() {
    TASK_MANAGER.mark_current_suspended();
}

/// Change the status of current `Running` task into `Exited`.
fn mark_current_exited() {
    TASK_MANAGER.mark_current_exited();
}

/// Suspend the current 'Running' task and run the next task in task list.
pub fn suspend_current_and_run_next() {
    mark_current_suspended();
    run_next_task();
}

/// Exit the current 'Running' task and run the next task in task list.
pub fn exit_current_and_run_next() {
    mark_current_exited();
    run_next_task();
}

/// Get the current 'Running' task's token.
pub fn current_user_token() -> usize {
    TASK_MANAGER.get_current_token()
}

/// Get the current 'Running' task's trap contexts.
pub fn current_trap_cx() -> &'static mut TrapContext {
    TASK_MANAGER.get_current_trap_cx()
}

/// Change the current 'Running' task's program break
pub fn change_program_brk(size: i32) -> Option<usize> {
    TASK_MANAGER.change_current_program_brk(size)
}

/// mark the schedule
pub fn schedule_mark() {
    TASK_MANAGER.schedule_mark();
}

/// record the syscall time
pub fn record_syscall_time(syscall_id: usize) {
    TASK_MANAGER.record_this_call(syscall_id);
}
/// 1
pub fn get_current_status() -> TaskStatus {
    TASK_MANAGER.get_current_status()
}
/// 1
pub fn get_currtask_first_scheduled_time() -> usize {
    TASK_MANAGER.get_currtask_first_scheduled_time()
}
/// 1
pub fn get_task_info() -> TaskInfo {
    let _status = TASK_MANAGER.get_current_status();
    let _syscall_times = TASK_MANAGER.get_currtask_syscall_time();
    let _time = get_time_ms() - TASK_MANAGER.get_currtask_first_scheduled_time();
    let res = TaskInfo {
        status: _status,
        syscall_times: _syscall_times,
        time: _time
    };
    res
}

/// 该函数用于开辟文件空间
pub fn mmap(start: usize, len: usize, prot: usize)->isize{
    TASK_MANAGER.mmap(start, len, prot)
}

/// 该函数用于释放文件空间
pub fn munmap(start: usize, len: usize) -> isize {
    TASK_MANAGER.munmap(start, len)
}