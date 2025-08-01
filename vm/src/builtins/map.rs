use super::{PyType, PyTypeRef};
use crate::{
    Context, Py, PyObjectRef, PyPayload, PyResult, VirtualMachine,
    builtins::PyTupleRef,
    class::PyClassImpl,
    function::PosArgs,
    protocol::{PyIter, PyIterReturn},
    raise_if_stop,
    types::{Constructor, IterNext, Iterable, SelfIter},
};

#[pyclass(module = false, name = "map", traverse)]
#[derive(Debug)]
pub struct PyMap {
    mapper: PyObjectRef,
    iterators: Vec<PyIter>,
}

impl PyPayload for PyMap {
    #[inline]
    fn class(ctx: &Context) -> &'static Py<PyType> {
        ctx.types.map_type
    }
}

impl Constructor for PyMap {
    type Args = (PyObjectRef, PosArgs<PyIter>);

    fn py_new(cls: PyTypeRef, (mapper, iterators): Self::Args, vm: &VirtualMachine) -> PyResult {
        let iterators = iterators.into_vec();
        Self { mapper, iterators }
            .into_ref_with_type(vm, cls)
            .map(Into::into)
    }
}

#[pyclass(with(IterNext, Iterable, Constructor), flags(BASETYPE))]
impl PyMap {
    #[pymethod]
    fn __length_hint__(&self, vm: &VirtualMachine) -> PyResult<usize> {
        self.iterators.iter().try_fold(0, |prev, cur| {
            let cur = cur.as_ref().to_owned().length_hint(0, vm)?;
            let max = std::cmp::max(prev, cur);
            Ok(max)
        })
    }

    #[pymethod]
    fn __reduce__(&self, vm: &VirtualMachine) -> (PyTypeRef, PyTupleRef) {
        let mut vec = vec![self.mapper.clone()];
        vec.extend(self.iterators.iter().map(|o| o.clone().into()));
        (vm.ctx.types.map_type.to_owned(), vm.new_tuple(vec))
    }
}

impl SelfIter for PyMap {}

impl IterNext for PyMap {
    fn next(zelf: &Py<Self>, vm: &VirtualMachine) -> PyResult<PyIterReturn> {
        let mut next_objs = Vec::new();
        for iterator in &zelf.iterators {
            let item = raise_if_stop!(iterator.next(vm)?);
            next_objs.push(item);
        }

        // the mapper itself can raise StopIteration which does stop the map iteration
        PyIterReturn::from_pyresult(zelf.mapper.call(next_objs, vm), vm)
    }
}

pub fn init(context: &Context) {
    PyMap::extend_class(context, context.types.map_type);
}
