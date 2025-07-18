/*
 * iterator types
 */

use super::{PyInt, PyTupleRef, PyType};
use crate::{
    Context, Py, PyObject, PyObjectRef, PyPayload, PyResult, VirtualMachine,
    class::PyClassImpl,
    function::ArgCallable,
    object::{Traverse, TraverseFn},
    protocol::{PyIterReturn, PySequence, PySequenceMethods},
    types::{IterNext, Iterable, SelfIter},
};
use rustpython_common::{
    lock::{PyMutex, PyRwLock, PyRwLockUpgradableReadGuard},
    static_cell,
};

/// Marks status of iterator.
#[derive(Debug, Clone)]
pub enum IterStatus<T> {
    /// Iterator hasn't raised StopIteration.
    Active(T),
    /// Iterator has raised StopIteration.
    Exhausted,
}

unsafe impl<T: Traverse> Traverse for IterStatus<T> {
    fn traverse(&self, tracer_fn: &mut TraverseFn<'_>) {
        match self {
            Self::Active(r) => r.traverse(tracer_fn),
            Self::Exhausted => (),
        }
    }
}

#[derive(Debug)]
pub struct PositionIterInternal<T> {
    pub status: IterStatus<T>,
    pub position: usize,
}

unsafe impl<T: Traverse> Traverse for PositionIterInternal<T> {
    fn traverse(&self, tracer_fn: &mut TraverseFn<'_>) {
        self.status.traverse(tracer_fn)
    }
}

impl<T> PositionIterInternal<T> {
    pub const fn new(obj: T, position: usize) -> Self {
        Self {
            status: IterStatus::Active(obj),
            position,
        }
    }

    pub fn set_state<F>(&mut self, state: PyObjectRef, f: F, vm: &VirtualMachine) -> PyResult<()>
    where
        F: FnOnce(&T, usize) -> usize,
    {
        if let IterStatus::Active(obj) = &self.status {
            if let Some(i) = state.downcast_ref::<PyInt>() {
                let i = i.try_to_primitive(vm).unwrap_or(0);
                self.position = f(obj, i);
                Ok(())
            } else {
                Err(vm.new_type_error("an integer is required."))
            }
        } else {
            Ok(())
        }
    }

    fn _reduce<F>(&self, func: PyObjectRef, f: F, vm: &VirtualMachine) -> PyTupleRef
    where
        F: FnOnce(&T) -> PyObjectRef,
    {
        if let IterStatus::Active(obj) = &self.status {
            vm.new_tuple((func, (f(obj),), self.position))
        } else {
            vm.new_tuple((func, (vm.ctx.new_list(Vec::new()),)))
        }
    }

    pub fn builtins_iter_reduce<F>(&self, f: F, vm: &VirtualMachine) -> PyTupleRef
    where
        F: FnOnce(&T) -> PyObjectRef,
    {
        let iter = builtins_iter(vm).to_owned();
        self._reduce(iter, f, vm)
    }

    pub fn builtins_reversed_reduce<F>(&self, f: F, vm: &VirtualMachine) -> PyTupleRef
    where
        F: FnOnce(&T) -> PyObjectRef,
    {
        let reversed = builtins_reversed(vm).to_owned();
        self._reduce(reversed, f, vm)
    }

    fn _next<F, OP>(&mut self, f: F, op: OP) -> PyResult<PyIterReturn>
    where
        F: FnOnce(&T, usize) -> PyResult<PyIterReturn>,
        OP: FnOnce(&mut Self),
    {
        if let IterStatus::Active(obj) = &self.status {
            let ret = f(obj, self.position);
            if let Ok(PyIterReturn::Return(_)) = ret {
                op(self);
            } else {
                self.status = IterStatus::Exhausted;
            }
            ret
        } else {
            Ok(PyIterReturn::StopIteration(None))
        }
    }

    pub fn next<F>(&mut self, f: F) -> PyResult<PyIterReturn>
    where
        F: FnOnce(&T, usize) -> PyResult<PyIterReturn>,
    {
        self._next(f, |zelf| zelf.position += 1)
    }

    pub fn rev_next<F>(&mut self, f: F) -> PyResult<PyIterReturn>
    where
        F: FnOnce(&T, usize) -> PyResult<PyIterReturn>,
    {
        self._next(f, |zelf| {
            if zelf.position == 0 {
                zelf.status = IterStatus::Exhausted;
            } else {
                zelf.position -= 1;
            }
        })
    }

    pub fn length_hint<F>(&self, f: F) -> usize
    where
        F: FnOnce(&T) -> usize,
    {
        if let IterStatus::Active(obj) = &self.status {
            f(obj).saturating_sub(self.position)
        } else {
            0
        }
    }

    pub fn rev_length_hint<F>(&self, f: F) -> usize
    where
        F: FnOnce(&T) -> usize,
    {
        if let IterStatus::Active(obj) = &self.status {
            if self.position <= f(obj) {
                return self.position + 1;
            }
        }
        0
    }
}

pub fn builtins_iter(vm: &VirtualMachine) -> &PyObject {
    static_cell! {
        static INSTANCE: PyObjectRef;
    }
    INSTANCE.get_or_init(|| vm.builtins.get_attr("iter", vm).unwrap())
}

pub fn builtins_reversed(vm: &VirtualMachine) -> &PyObject {
    static_cell! {
        static INSTANCE: PyObjectRef;
    }
    INSTANCE.get_or_init(|| vm.builtins.get_attr("reversed", vm).unwrap())
}

#[pyclass(module = false, name = "iterator", traverse)]
#[derive(Debug)]
pub struct PySequenceIterator {
    // cached sequence methods
    #[pytraverse(skip)]
    seq_methods: &'static PySequenceMethods,
    internal: PyMutex<PositionIterInternal<PyObjectRef>>,
}

impl PyPayload for PySequenceIterator {
    #[inline]
    fn class(ctx: &Context) -> &'static Py<PyType> {
        ctx.types.iter_type
    }
}

#[pyclass(with(IterNext, Iterable))]
impl PySequenceIterator {
    pub fn new(obj: PyObjectRef, vm: &VirtualMachine) -> PyResult<Self> {
        let seq = PySequence::try_protocol(&obj, vm)?;
        Ok(Self {
            seq_methods: seq.methods,
            internal: PyMutex::new(PositionIterInternal::new(obj, 0)),
        })
    }

    #[pymethod]
    fn __length_hint__(&self, vm: &VirtualMachine) -> PyObjectRef {
        let internal = self.internal.lock();
        if let IterStatus::Active(obj) = &internal.status {
            let seq = PySequence {
                obj,
                methods: self.seq_methods,
            };
            seq.length(vm)
                .map(|x| PyInt::from(x).into_pyobject(vm))
                .unwrap_or_else(|_| vm.ctx.not_implemented())
        } else {
            PyInt::from(0).into_pyobject(vm)
        }
    }

    #[pymethod]
    fn __reduce__(&self, vm: &VirtualMachine) -> PyTupleRef {
        self.internal.lock().builtins_iter_reduce(|x| x.clone(), vm)
    }

    #[pymethod]
    fn __setstate__(&self, state: PyObjectRef, vm: &VirtualMachine) -> PyResult<()> {
        self.internal.lock().set_state(state, |_, pos| pos, vm)
    }
}

impl SelfIter for PySequenceIterator {}
impl IterNext for PySequenceIterator {
    fn next(zelf: &Py<Self>, vm: &VirtualMachine) -> PyResult<PyIterReturn> {
        zelf.internal.lock().next(|obj, pos| {
            let seq = PySequence {
                obj,
                methods: zelf.seq_methods,
            };
            PyIterReturn::from_getitem_result(seq.get_item(pos as isize, vm), vm)
        })
    }
}

#[pyclass(module = false, name = "callable_iterator", traverse)]
#[derive(Debug)]
pub struct PyCallableIterator {
    sentinel: PyObjectRef,
    status: PyRwLock<IterStatus<ArgCallable>>,
}

impl PyPayload for PyCallableIterator {
    #[inline]
    fn class(ctx: &Context) -> &'static Py<PyType> {
        ctx.types.callable_iterator
    }
}

#[pyclass(with(IterNext, Iterable))]
impl PyCallableIterator {
    pub const fn new(callable: ArgCallable, sentinel: PyObjectRef) -> Self {
        Self {
            sentinel,
            status: PyRwLock::new(IterStatus::Active(callable)),
        }
    }
}

impl SelfIter for PyCallableIterator {}
impl IterNext for PyCallableIterator {
    fn next(zelf: &Py<Self>, vm: &VirtualMachine) -> PyResult<PyIterReturn> {
        let status = zelf.status.upgradable_read();
        let next = if let IterStatus::Active(callable) = &*status {
            let ret = callable.invoke((), vm)?;
            if vm.bool_eq(&ret, &zelf.sentinel)? {
                *PyRwLockUpgradableReadGuard::upgrade(status) = IterStatus::Exhausted;
                PyIterReturn::StopIteration(None)
            } else {
                PyIterReturn::Return(ret)
            }
        } else {
            PyIterReturn::StopIteration(None)
        };
        Ok(next)
    }
}

pub fn init(context: &Context) {
    PySequenceIterator::extend_class(context, context.types.iter_type);
    PyCallableIterator::extend_class(context, context.types.callable_iterator);
}
