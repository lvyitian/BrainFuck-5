use std::collections::VecDeque;
use std::fmt;
use std::mem;
use std::ops::{Index, IndexMut};

use super::super::code_gen;
use super::super::parser::ASTNode;

use libc::{sysconf, _SC_PAGESIZE};

use runnable::Runnable;

lazy_static! {
    static ref PAGE_SIZE: usize = unsafe { sysconf(_SC_PAGESIZE) as usize };
}

/// Functions called by JIT-compiled code.
mod jit_functions {
    use libc::{c_int, getchar, putchar};

    /// Print a single byte to stdout.
    pub extern "C" fn print(byte: u8) {
        unsafe {
            putchar(byte as c_int);
        }
    }

    /// Read a single byte from stdin.
    pub extern "C" fn read() -> u8 {
        unsafe { getchar() as u8 }
    }
}

/// Round up an integer division.
///
/// * `numerator` - The upper component of a division
/// * `denominator` - The lower component of a division
fn int_ceil(numerator: usize, denominator: usize) -> usize {
    (numerator / denominator + 1) * denominator
}

/// Clone a vector of bytes into new executable memory pages.
fn make_executable(source: &Vec<u8>) -> Vec<u8> {
    let size = int_ceil(source.len(), *PAGE_SIZE);
    let mut data: Vec<u8>;

    unsafe {
        let mut ptr: *mut libc::c_void = mem::MaybeUninit::uninit().assume_init();

        libc::posix_memalign(&mut ptr, *PAGE_SIZE, size);
        libc::mprotect(
            ptr,
            size,
            libc::PROT_EXEC | libc::PROT_READ | libc::PROT_WRITE,
        );
        libc::memset(ptr, 0xc3, size); // for now, prepopulate with 'RET'

        data = Vec::from_raw_parts(ptr as *mut u8, source.len(), size);
    }

    for (index, &byte) in source.iter().enumerate() {
        data[index] = byte;
    }

    data
}

pub type JITPromiseID = usize;

/// Holds ASTNodes for later compilation.
#[derive(Debug)]
pub enum JITPromise {
    Deferred(VecDeque<ASTNode>),
    Compiled(JITTarget),
}

/// Container for executable bytes.
pub struct JITTarget {
    bytes: Vec<u8>,
    loops: Vec<JITPromise>,
}

impl JITTarget {
    /// Initialize a JIT compiled version of a program.
    #[cfg(target_arch = "x86_64")]
    pub fn new(nodes: &VecDeque<ASTNode>) -> Result<Self, String> {
        let mut bytes = Vec::new();
        let mut loops = Vec::new();

        code_gen::wrapper(&mut bytes, Self::shallow_compile(nodes, &mut loops));

        Ok(Self {
            bytes: make_executable(&bytes),
            loops: loops,
        })
    }

    /// No-op version for unsupported architectures.
    #[cfg(not(target_arch = "x86_64"))]
    pub fn new(&self) -> Result<Self, String> {
        Err(format!("Unsupported JIT architecture."))
    }

    fn new_fragment(nodes: &VecDeque<ASTNode>) -> Self {
        let mut bytes = Vec::new();
        let mut loops = Vec::new();

        code_gen::wrapper(&mut bytes, Self::compile_loop(nodes, &mut loops));

        Self {
            bytes: make_executable(&bytes),
            loops: loops,
        }
    }

    /// Convert a vector of ASTNodes into a sequence of executable bytes.
    ///
    /// r10 is used to hold the data pointer.
    fn shallow_compile(nodes: &VecDeque<ASTNode>, loops: &mut Vec<JITPromise>) -> Vec<u8> {
        let mut bytes = Vec::new();

        for node in nodes {
            match node {
                ASTNode::Incr(n) => code_gen::incr(&mut bytes, *n),
                ASTNode::Decr(n) => code_gen::decr(&mut bytes, *n),
                ASTNode::Next(n) => code_gen::next(&mut bytes, *n),
                ASTNode::Prev(n) => code_gen::prev(&mut bytes, *n),
                ASTNode::Print => code_gen::print(&mut bytes, jit_functions::print),
                ASTNode::Read => code_gen::read(&mut bytes, jit_functions::read),
                ASTNode::Loop(nodes) if nodes.len() > 0x16 => {
                    bytes.extend(Self::defer_loop(nodes, loops))
                }
                ASTNode::Loop(nodes) => bytes.extend(Self::compile_loop(nodes, loops)),
            };
        }

        bytes
    }

    /// Perform AOT compilation on a loop.
    fn compile_loop(nodes: &VecDeque<ASTNode>, loops: &mut Vec<JITPromise>) -> Vec<u8> {
        let mut bytes = Vec::new();

        code_gen::aot_loop(&mut bytes, Self::shallow_compile(nodes, loops));

        bytes
    }

    /// Perform JIT compilation on a loop.
    fn defer_loop(nodes: &VecDeque<ASTNode>, loops: &mut Vec<JITPromise>) -> Vec<u8> {
        let mut bytes = Vec::new();

        loops.push(JITPromise::Deferred(nodes.clone()));

        code_gen::jit_loop(&mut bytes, loops.len() - 1);

        bytes
    }

    /// Execute the bytes buffer as a function with context.
    fn exec(&mut self, mem_ptr: *mut u8) -> *mut u8 {
        let jit_callback_ptr = Self::jit_callback;

        type JITCallbackType = extern "C" fn(&mut JITTarget, JITPromiseID, *mut u8) -> *mut u8;
        let func: fn(*mut u8, *mut JITTarget, JITCallbackType) -> *mut u8 =
            unsafe { mem::transmute(self.bytes.as_mut_ptr()) };

        func(mem_ptr, self, jit_callback_ptr)
    }

    /// Callback passed into compiled code. Allows for deferred compilation
    /// targets to be compiled, ran, and later re-ran.
    extern "C" fn jit_callback(&mut self, loop_index: JITPromiseID, mem_ptr: *mut u8) -> *mut u8 {
        let jit_promise = &mut self.loops[loop_index];
        let return_ptr;

        match jit_promise {
            JITPromise::Deferred(nodes) => {
                let mut new_target = Self::new_fragment(nodes);
                return_ptr = new_target.exec(mem_ptr);
                *jit_promise = JITPromise::Compiled(new_target);
            }
            JITPromise::Compiled(jit_target) => {
                return_ptr = jit_target.exec(mem_ptr);
            }
        };

        return_ptr
    }
}

impl Runnable for JITTarget {
    fn run(&mut self) {
        let mut bf_mem = vec![0u8; 30_000]; // Memory space used by BrainFuck
        let mem_ptr = bf_mem.as_mut_ptr();

        self.exec(mem_ptr);
    }
}

impl Index<usize> for JITTarget {
    type Output = u8;

    fn index(&self, index: usize) -> &u8 {
        &self.bytes[index]
    }
}

impl IndexMut<usize> for JITTarget {
    fn index_mut(&mut self, index: usize) -> &mut u8 {
        &mut self.bytes[index]
    }
}

/// Display hexadecimal values for data.
impl fmt::Debug for JITTarget {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for byte in self.bytes.iter() {
            write!(f, "{:02X}", byte)?;
        }

        writeln!(f)
    }
}