use super::{
    IterStatus, PositionIterInternal, PyBaseExceptionRef, PyGenericAlias, PyMappingProxy, PySet,
    PyStr, PyStrRef, PyTupleRef, PyType, PyTypeRef, set::PySetInner,
};
use crate::{
    AsObject, Context, Py, PyObject, PyObjectRef, PyPayload, PyRef, PyRefExact, PyResult,
    TryFromObject, atomic_func,
    builtins::{
        PyTuple,
        iter::{builtins_iter, builtins_reversed},
        type_::PyAttributes,
    },
    class::{PyClassDef, PyClassImpl},
    common::ascii,
    dict_inner::{self, DictKey},
    function::{ArgIterable, KwArgs, OptionalArg, PyArithmeticValue::*, PyComparisonValue},
    iter::PyExactSizeIterator,
    protocol::{PyIterIter, PyIterReturn, PyMappingMethods, PyNumberMethods, PySequenceMethods},
    recursion::ReprGuard,
    types::{
        AsMapping, AsNumber, AsSequence, Callable, Comparable, Constructor, DefaultConstructor,
        Initializer, IterNext, Iterable, PyComparisonOp, Representable, SelfIter, Unconstructible,
    },
    vm::VirtualMachine,
};
use rustpython_common::lock::PyMutex;
use std::fmt;
use std::sync::LazyLock;

pub type DictContentType = dict_inner::Dict;

#[pyclass(module = false, name = "dict", unhashable = true, traverse)]
#[derive(Default)]
pub struct PyDict {
    entries: DictContentType,
}
pub type PyDictRef = PyRef<PyDict>;

impl fmt::Debug for PyDict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: implement more detailed, non-recursive Debug formatter
        f.write_str("dict")
    }
}

impl PyPayload for PyDict {
    #[inline]
    fn class(ctx: &Context) -> &'static Py<PyType> {
        ctx.types.dict_type
    }
}

impl PyDict {
    #[deprecated(note = "use PyDict::default().into_ref() instead")]
    pub fn new_ref(ctx: &Context) -> PyRef<Self> {
        Self::default().into_ref(ctx)
    }

    /// escape hatch to access the underlying data structure directly. prefer adding a method on
    /// PyDict instead of using this
    pub(crate) const fn _as_dict_inner(&self) -> &DictContentType {
        &self.entries
    }

    // Used in update and ior.
    pub(crate) fn merge_object(&self, other: PyObjectRef, vm: &VirtualMachine) -> PyResult<()> {
        let casted: Result<PyRefExact<Self>, _> = other.downcast_exact(vm);
        let other = match casted {
            Ok(dict_other) => return self.merge_dict(dict_other.into_pyref(), vm),
            Err(other) => other,
        };
        let dict = &self.entries;
        if let Some(keys) = vm.get_method(other.clone(), vm.ctx.intern_str("keys")) {
            let keys = keys?.call((), vm)?.get_iter(vm)?;
            while let PyIterReturn::Return(key) = keys.next(vm)? {
                let val = other.get_item(&*key, vm)?;
                dict.insert(vm, &*key, val)?;
            }
        } else {
            let iter = other.get_iter(vm)?;
            loop {
                fn err(vm: &VirtualMachine) -> PyBaseExceptionRef {
                    vm.new_value_error("Iterator must have exactly two elements")
                }
                let element = match iter.next(vm)? {
                    PyIterReturn::Return(obj) => obj,
                    PyIterReturn::StopIteration(_) => break,
                };
                let elem_iter = element.get_iter(vm)?;
                let key = elem_iter.next(vm)?.into_result().map_err(|_| err(vm))?;
                let value = elem_iter.next(vm)?.into_result().map_err(|_| err(vm))?;
                if matches!(elem_iter.next(vm)?, PyIterReturn::Return(_)) {
                    return Err(err(vm));
                }
                dict.insert(vm, &*key, value)?;
            }
        }
        Ok(())
    }

    fn merge_dict(&self, dict_other: PyDictRef, vm: &VirtualMachine) -> PyResult<()> {
        let dict = &self.entries;
        let dict_size = &dict_other.size();
        for (key, value) in &dict_other {
            dict.insert(vm, &*key, value)?;
        }
        if dict_other.entries.has_changed_size(dict_size) {
            return Err(vm.new_runtime_error("dict mutated during update"));
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.len() == 0
    }

    /// Set item variant which can be called with multiple
    /// key types, such as str to name a notable one.
    pub(crate) fn inner_setitem<K: DictKey + ?Sized>(
        &self,
        key: &K,
        value: PyObjectRef,
        vm: &VirtualMachine,
    ) -> PyResult<()> {
        self.entries.insert(vm, key, value)
    }

    pub(crate) fn inner_delitem<K: DictKey + ?Sized>(
        &self,
        key: &K,
        vm: &VirtualMachine,
    ) -> PyResult<()> {
        self.entries.delete(vm, key)
    }

    pub fn get_or_insert(
        &self,
        vm: &VirtualMachine,
        key: PyObjectRef,
        default: impl FnOnce() -> PyObjectRef,
    ) -> PyResult {
        self.entries.setdefault(vm, &*key, default)
    }

    pub fn from_attributes(attrs: PyAttributes, vm: &VirtualMachine) -> PyResult<Self> {
        let entries = DictContentType::default();

        for (key, value) in attrs {
            entries.insert(vm, key, value)?;
        }

        Ok(Self { entries })
    }

    pub fn contains_key<K: DictKey + ?Sized>(&self, key: &K, vm: &VirtualMachine) -> bool {
        self.entries.contains(vm, key).unwrap()
    }

    pub fn size(&self) -> dict_inner::DictSize {
        self.entries.size()
    }
}

// Python dict methods:
#[allow(clippy::len_without_is_empty)]
#[pyclass(
    with(
        Py,
        PyRef,
        Constructor,
        Initializer,
        Comparable,
        Iterable,
        AsSequence,
        AsNumber,
        AsMapping,
        Representable
    ),
    flags(BASETYPE)
)]
impl PyDict {
    #[pyclassmethod]
    fn fromkeys(
        class: PyTypeRef,
        iterable: ArgIterable,
        value: OptionalArg<PyObjectRef>,
        vm: &VirtualMachine,
    ) -> PyResult {
        let value = value.unwrap_or_none(vm);
        let d = PyType::call(&class, ().into(), vm)?;
        match d.downcast_exact::<Self>(vm) {
            Ok(pydict) => {
                for key in iterable.iter(vm)? {
                    pydict.__setitem__(key?, value.clone(), vm)?;
                }
                Ok(pydict.into_pyref().into())
            }
            Err(pyobj) => {
                for key in iterable.iter(vm)? {
                    pyobj.set_item(&*key?, value.clone(), vm)?;
                }
                Ok(pyobj)
            }
        }
    }

    #[pymethod]
    pub fn __len__(&self) -> usize {
        self.entries.len()
    }

    #[pymethod]
    fn __sizeof__(&self) -> usize {
        std::mem::size_of::<Self>() + self.entries.sizeof()
    }

    #[pymethod]
    fn __contains__(&self, key: PyObjectRef, vm: &VirtualMachine) -> PyResult<bool> {
        self.entries.contains(vm, &*key)
    }

    #[pymethod]
    fn __delitem__(&self, key: PyObjectRef, vm: &VirtualMachine) -> PyResult<()> {
        self.inner_delitem(&*key, vm)
    }

    #[pymethod]
    pub fn clear(&self) {
        self.entries.clear()
    }

    #[pymethod]
    fn __setitem__(
        &self,
        key: PyObjectRef,
        value: PyObjectRef,
        vm: &VirtualMachine,
    ) -> PyResult<()> {
        self.inner_setitem(&*key, value, vm)
    }

    #[pymethod]
    fn get(
        &self,
        key: PyObjectRef,
        default: OptionalArg<PyObjectRef>,
        vm: &VirtualMachine,
    ) -> PyResult {
        match self.entries.get(vm, &*key)? {
            Some(value) => Ok(value),
            None => Ok(default.unwrap_or_none(vm)),
        }
    }

    #[pymethod]
    fn setdefault(
        &self,
        key: PyObjectRef,
        default: OptionalArg<PyObjectRef>,
        vm: &VirtualMachine,
    ) -> PyResult {
        self.entries
            .setdefault(vm, &*key, || default.unwrap_or_none(vm))
    }

    #[pymethod]
    pub fn copy(&self) -> Self {
        Self {
            entries: self.entries.clone(),
        }
    }

    #[pymethod]
    fn update(
        &self,
        dict_obj: OptionalArg<PyObjectRef>,
        kwargs: KwArgs,
        vm: &VirtualMachine,
    ) -> PyResult<()> {
        if let OptionalArg::Present(dict_obj) = dict_obj {
            self.merge_object(dict_obj, vm)?;
        }
        for (key, value) in kwargs {
            self.entries.insert(vm, &key, value)?;
        }
        Ok(())
    }

    #[pymethod]
    fn __or__(&self, other: PyObjectRef, vm: &VirtualMachine) -> PyResult {
        let other_dict: Result<PyDictRef, _> = other.downcast();
        if let Ok(other) = other_dict {
            let self_cp = self.copy();
            self_cp.merge_dict(other, vm)?;
            return Ok(self_cp.into_pyobject(vm));
        }
        Ok(vm.ctx.not_implemented())
    }

    #[pymethod]
    fn pop(
        &self,
        key: PyObjectRef,
        default: OptionalArg<PyObjectRef>,
        vm: &VirtualMachine,
    ) -> PyResult {
        match self.entries.pop(vm, &*key)? {
            Some(value) => Ok(value),
            None => default.ok_or_else(|| vm.new_key_error(key)),
        }
    }

    #[pymethod]
    fn popitem(&self, vm: &VirtualMachine) -> PyResult<(PyObjectRef, PyObjectRef)> {
        let (key, value) = self.entries.pop_back().ok_or_else(|| {
            let err_msg = vm
                .ctx
                .new_str(ascii!("popitem(): dictionary is empty"))
                .into();
            vm.new_key_error(err_msg)
        })?;
        Ok((key, value))
    }

    #[pyclassmethod]
    fn __class_getitem__(cls: PyTypeRef, args: PyObjectRef, vm: &VirtualMachine) -> PyGenericAlias {
        PyGenericAlias::from_args(cls, args, vm)
    }
}

#[pyclass]
impl Py<PyDict> {
    fn inner_cmp(
        &self,
        other: &Self,
        op: PyComparisonOp,
        item: bool,
        vm: &VirtualMachine,
    ) -> PyResult<PyComparisonValue> {
        if op == PyComparisonOp::Ne {
            return Self::inner_cmp(self, other, PyComparisonOp::Eq, item, vm)
                .map(|x| x.map(|eq| !eq));
        }
        if !op.eval_ord(self.__len__().cmp(&other.__len__())) {
            return Ok(Implemented(false));
        }
        let (superset, subset) = if self.__len__() < other.__len__() {
            (other, self)
        } else {
            (self, other)
        };
        for (k, v1) in subset {
            match superset.get_item_opt(&*k, vm)? {
                Some(v2) => {
                    if v1.is(&v2) {
                        continue;
                    }
                    if item && !vm.bool_eq(&v1, &v2)? {
                        return Ok(Implemented(false));
                    }
                }
                None => {
                    return Ok(Implemented(false));
                }
            }
        }
        Ok(Implemented(true))
    }

    #[pymethod]
    #[cfg_attr(feature = "flame-it", flame("PyDictRef"))]
    fn __getitem__(&self, key: PyObjectRef, vm: &VirtualMachine) -> PyResult {
        self.inner_getitem(&*key, vm)
    }
}

#[pyclass]
impl PyRef<PyDict> {
    #[pymethod]
    const fn keys(self) -> PyDictKeys {
        PyDictKeys::new(self)
    }

    #[pymethod]
    const fn values(self) -> PyDictValues {
        PyDictValues::new(self)
    }

    #[pymethod]
    const fn items(self) -> PyDictItems {
        PyDictItems::new(self)
    }

    #[pymethod]
    fn __reversed__(self) -> PyDictReverseKeyIterator {
        PyDictReverseKeyIterator::new(self)
    }

    #[pymethod]
    fn __ior__(self, other: PyObjectRef, vm: &VirtualMachine) -> PyResult<Self> {
        self.merge_object(other, vm)?;
        Ok(self)
    }

    #[pymethod]
    fn __ror__(self, other: PyObjectRef, vm: &VirtualMachine) -> PyResult {
        let other_dict: Result<Self, _> = other.downcast();
        if let Ok(other) = other_dict {
            let other_cp = other.copy();
            other_cp.merge_dict(self, vm)?;
            return Ok(other_cp.into_pyobject(vm));
        }
        Ok(vm.ctx.not_implemented())
    }
}

impl DefaultConstructor for PyDict {}

impl Initializer for PyDict {
    type Args = (OptionalArg<PyObjectRef>, KwArgs);

    fn init(
        zelf: PyRef<Self>,
        (dict_obj, kwargs): Self::Args,
        vm: &VirtualMachine,
    ) -> PyResult<()> {
        zelf.update(dict_obj, kwargs, vm)
    }
}

impl AsMapping for PyDict {
    fn as_mapping() -> &'static PyMappingMethods {
        static AS_MAPPING: PyMappingMethods = PyMappingMethods {
            length: atomic_func!(|mapping, _vm| Ok(PyDict::mapping_downcast(mapping).__len__())),
            subscript: atomic_func!(|mapping, needle, vm| {
                PyDict::mapping_downcast(mapping).inner_getitem(needle, vm)
            }),
            ass_subscript: atomic_func!(|mapping, needle, value, vm| {
                let zelf = PyDict::mapping_downcast(mapping);
                if let Some(value) = value {
                    zelf.inner_setitem(needle, value, vm)
                } else {
                    zelf.inner_delitem(needle, vm)
                }
            }),
        };
        &AS_MAPPING
    }
}

impl AsSequence for PyDict {
    fn as_sequence() -> &'static PySequenceMethods {
        static AS_SEQUENCE: LazyLock<PySequenceMethods> = LazyLock::new(|| PySequenceMethods {
            contains: atomic_func!(|seq, target, vm| PyDict::sequence_downcast(seq)
                .entries
                .contains(vm, target)),
            ..PySequenceMethods::NOT_IMPLEMENTED
        });
        &AS_SEQUENCE
    }
}

impl AsNumber for PyDict {
    fn as_number() -> &'static PyNumberMethods {
        static AS_NUMBER: PyNumberMethods = PyNumberMethods {
            or: Some(|a, b, vm| {
                if let Some(a) = a.downcast_ref::<PyDict>() {
                    PyDict::__or__(a, b.to_pyobject(vm), vm)
                } else {
                    Ok(vm.ctx.not_implemented())
                }
            }),
            inplace_or: Some(|a, b, vm| {
                if let Some(a) = a.downcast_ref::<PyDict>() {
                    a.to_owned()
                        .__ior__(b.to_pyobject(vm), vm)
                        .map(|d| d.into())
                } else {
                    Ok(vm.ctx.not_implemented())
                }
            }),
            ..PyNumberMethods::NOT_IMPLEMENTED
        };
        &AS_NUMBER
    }
}

impl Comparable for PyDict {
    fn cmp(
        zelf: &Py<Self>,
        other: &PyObject,
        op: PyComparisonOp,
        vm: &VirtualMachine,
    ) -> PyResult<PyComparisonValue> {
        op.eq_only(|| {
            let other = class_or_notimplemented!(Self, other);
            zelf.inner_cmp(other, PyComparisonOp::Eq, true, vm)
        })
    }
}

impl Iterable for PyDict {
    fn iter(zelf: PyRef<Self>, vm: &VirtualMachine) -> PyResult {
        Ok(PyDictKeyIterator::new(zelf).into_pyobject(vm))
    }
}

impl Representable for PyDict {
    #[inline]
    fn repr(zelf: &Py<Self>, vm: &VirtualMachine) -> PyResult<PyStrRef> {
        let s = if let Some(_guard) = ReprGuard::enter(vm, zelf.as_object()) {
            let mut str_parts = Vec::with_capacity(zelf.__len__());
            for (key, value) in zelf {
                let key_repr = &key.repr(vm)?;
                let value_repr = value.repr(vm)?;
                str_parts.push(format!("{key_repr}: {value_repr}"));
            }

            vm.ctx.new_str(format!("{{{}}}", str_parts.join(", ")))
        } else {
            vm.ctx.intern_str("{...}").to_owned()
        };
        Ok(s)
    }

    #[cold]
    fn repr_str(_zelf: &Py<Self>, _vm: &VirtualMachine) -> PyResult<String> {
        unreachable!("use repr instead")
    }
}

impl Py<PyDict> {
    #[inline]
    fn exact_dict(&self, vm: &VirtualMachine) -> bool {
        self.class().is(vm.ctx.types.dict_type)
    }

    fn missing_opt<K: DictKey + ?Sized>(
        &self,
        key: &K,
        vm: &VirtualMachine,
    ) -> PyResult<Option<PyObjectRef>> {
        vm.get_method(self.to_owned().into(), identifier!(vm, __missing__))
            .map(|methods| methods?.call((key.to_pyobject(vm),), vm))
            .transpose()
    }

    #[inline]
    fn inner_getitem<K: DictKey + ?Sized>(
        &self,
        key: &K,
        vm: &VirtualMachine,
    ) -> PyResult<PyObjectRef> {
        if let Some(value) = self.entries.get(vm, key)? {
            Ok(value)
        } else if let Some(value) = self.missing_opt(key, vm)? {
            Ok(value)
        } else {
            Err(vm.new_key_error(key.to_pyobject(vm)))
        }
    }

    /// Take a python dictionary and convert it to attributes.
    pub fn to_attributes(&self, vm: &VirtualMachine) -> PyAttributes {
        let mut attrs = PyAttributes::default();
        for (key, value) in self {
            let key: PyRefExact<PyStr> = key.downcast_exact(vm).expect("dict has non-string keys");
            attrs.insert(vm.ctx.intern_str(key), value);
        }
        attrs
    }

    pub fn get_item_opt<K: DictKey + ?Sized>(
        &self,
        key: &K,
        vm: &VirtualMachine,
    ) -> PyResult<Option<PyObjectRef>> {
        if self.exact_dict(vm) {
            self.entries.get(vm, key)
            // FIXME: check __missing__?
        } else {
            match self.as_object().get_item(key, vm) {
                Ok(value) => Ok(Some(value)),
                Err(e) if e.fast_isinstance(vm.ctx.exceptions.key_error) => {
                    self.missing_opt(key, vm)
                }
                Err(e) => Err(e),
            }
        }
    }

    pub fn get_item<K: DictKey + ?Sized>(&self, key: &K, vm: &VirtualMachine) -> PyResult {
        if self.exact_dict(vm) {
            self.inner_getitem(key, vm)
        } else {
            self.as_object().get_item(key, vm)
        }
    }

    pub fn set_item<K: DictKey + ?Sized>(
        &self,
        key: &K,
        value: PyObjectRef,
        vm: &VirtualMachine,
    ) -> PyResult<()> {
        if self.exact_dict(vm) {
            self.inner_setitem(key, value, vm)
        } else {
            self.as_object().set_item(key, value, vm)
        }
    }

    pub fn del_item<K: DictKey + ?Sized>(&self, key: &K, vm: &VirtualMachine) -> PyResult<()> {
        if self.exact_dict(vm) {
            self.inner_delitem(key, vm)
        } else {
            self.as_object().del_item(key, vm)
        }
    }

    pub fn pop_item<K: DictKey + ?Sized>(
        &self,
        key: &K,
        vm: &VirtualMachine,
    ) -> PyResult<Option<PyObjectRef>> {
        if self.exact_dict(vm) {
            self.entries.remove_if_exists(vm, key)
        } else {
            let value = self.as_object().get_item(key, vm)?;
            self.as_object().del_item(key, vm)?;
            Ok(Some(value))
        }
    }

    pub fn get_chain<K: DictKey + ?Sized>(
        &self,
        other: &Self,
        key: &K,
        vm: &VirtualMachine,
    ) -> PyResult<Option<PyObjectRef>> {
        let self_exact = self.exact_dict(vm);
        let other_exact = other.exact_dict(vm);
        if self_exact && other_exact {
            self.entries.get_chain(&other.entries, vm, key)
        } else if let Some(value) = self.get_item_opt(key, vm)? {
            Ok(Some(value))
        } else {
            other.get_item_opt(key, vm)
        }
    }
}

// Implement IntoIterator so that we can easily iterate dictionaries from rust code.
impl IntoIterator for PyDictRef {
    type Item = (PyObjectRef, PyObjectRef);
    type IntoIter = DictIntoIter;

    fn into_iter(self) -> Self::IntoIter {
        DictIntoIter::new(self)
    }
}

impl<'a> IntoIterator for &'a PyDictRef {
    type Item = (PyObjectRef, PyObjectRef);
    type IntoIter = DictIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        DictIter::new(self)
    }
}

impl<'a> IntoIterator for &'a Py<PyDict> {
    type Item = (PyObjectRef, PyObjectRef);
    type IntoIter = DictIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        DictIter::new(self)
    }
}

impl<'a> IntoIterator for &'a PyDict {
    type Item = (PyObjectRef, PyObjectRef);
    type IntoIter = DictIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        DictIter::new(self)
    }
}

pub struct DictIntoIter {
    dict: PyDictRef,
    position: usize,
}

impl DictIntoIter {
    pub const fn new(dict: PyDictRef) -> Self {
        Self { dict, position: 0 }
    }
}

impl Iterator for DictIntoIter {
    type Item = (PyObjectRef, PyObjectRef);

    fn next(&mut self) -> Option<Self::Item> {
        let (position, key, value) = self.dict.entries.next_entry(self.position)?;
        self.position = position;
        Some((key, value))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let l = self.len();
        (l, Some(l))
    }
}
impl ExactSizeIterator for DictIntoIter {
    fn len(&self) -> usize {
        self.dict.entries.len_from_entry_index(self.position)
    }
}

pub struct DictIter<'a> {
    dict: &'a PyDict,
    position: usize,
}

impl<'a> DictIter<'a> {
    pub const fn new(dict: &'a PyDict) -> Self {
        DictIter { dict, position: 0 }
    }
}

impl Iterator for DictIter<'_> {
    type Item = (PyObjectRef, PyObjectRef);

    fn next(&mut self) -> Option<Self::Item> {
        let (position, key, value) = self.dict.entries.next_entry(self.position)?;
        self.position = position;
        Some((key, value))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let l = self.len();
        (l, Some(l))
    }
}
impl ExactSizeIterator for DictIter<'_> {
    fn len(&self) -> usize {
        self.dict.entries.len_from_entry_index(self.position)
    }
}

#[pyclass]
trait DictView: PyPayload + PyClassDef + Iterable + Representable {
    type ReverseIter: PyPayload;

    fn dict(&self) -> &PyDictRef;
    fn item(vm: &VirtualMachine, key: PyObjectRef, value: PyObjectRef) -> PyObjectRef;

    #[pymethod]
    fn __len__(&self) -> usize {
        self.dict().__len__()
    }

    #[pymethod]
    fn __reversed__(&self) -> Self::ReverseIter;
}

macro_rules! dict_view {
    ( $name: ident, $iter_name: ident, $reverse_iter_name: ident,
      $class: ident, $iter_class: ident, $reverse_iter_class: ident,
      $class_name: literal, $iter_class_name: literal, $reverse_iter_class_name: literal,
      $result_fn: expr) => {
        #[pyclass(module = false, name = $class_name)]
        #[derive(Debug)]
        pub(crate) struct $name {
            pub dict: PyDictRef,
        }

        impl $name {
            pub const fn new(dict: PyDictRef) -> Self {
                $name { dict }
            }
        }

        impl DictView for $name {
            type ReverseIter = $reverse_iter_name;

            fn dict(&self) -> &PyDictRef {
                &self.dict
            }

            fn item(vm: &VirtualMachine, key: PyObjectRef, value: PyObjectRef) -> PyObjectRef {
                #[allow(clippy::redundant_closure_call)]
                $result_fn(vm, key, value)
            }

            fn __reversed__(&self) -> Self::ReverseIter {
                $reverse_iter_name::new(self.dict.clone())
            }
        }

        impl Iterable for $name {
            fn iter(zelf: PyRef<Self>, vm: &VirtualMachine) -> PyResult {
                Ok($iter_name::new(zelf.dict.clone()).into_pyobject(vm))
            }
        }

        impl PyPayload for $name {
            fn class(ctx: &Context) -> &'static Py<PyType> {
                ctx.types.$class
            }
        }

        impl Representable for $name {
            #[inline]
            fn repr(zelf: &Py<Self>, vm: &VirtualMachine) -> PyResult<PyStrRef> {
                let s = if let Some(_guard) = ReprGuard::enter(vm, zelf.as_object()) {
                    let mut str_parts = Vec::with_capacity(zelf.__len__());
                    for (key, value) in zelf.dict().clone() {
                        let s = &Self::item(vm, key, value).repr(vm)?;
                        str_parts.push(s.as_str().to_owned());
                    }
                    vm.ctx
                        .new_str(format!("{}([{}])", Self::NAME, str_parts.join(", ")))
                } else {
                    vm.ctx.intern_str("{...}").to_owned()
                };
                Ok(s)
            }

            #[cold]
            fn repr_str(_zelf: &Py<Self>, _vm: &VirtualMachine) -> PyResult<String> {
                unreachable!("use repr instead")
            }
        }

        #[pyclass(module = false, name = $iter_class_name)]
        #[derive(Debug)]
        pub(crate) struct $iter_name {
            pub size: dict_inner::DictSize,
            pub internal: PyMutex<PositionIterInternal<PyDictRef>>,
        }

        impl PyPayload for $iter_name {
            #[inline]
            fn class(ctx: &Context) -> &'static Py<PyType> {
                ctx.types.$iter_class
            }
        }

        #[pyclass(with(Unconstructible, IterNext, Iterable))]
        impl $iter_name {
            fn new(dict: PyDictRef) -> Self {
                $iter_name {
                    size: dict.size(),
                    internal: PyMutex::new(PositionIterInternal::new(dict, 0)),
                }
            }

            #[pymethod]
            fn __length_hint__(&self) -> usize {
                self.internal.lock().length_hint(|_| self.size.entries_size)
            }

            #[allow(clippy::redundant_closure_call)]
            #[pymethod]
            fn __reduce__(&self, vm: &VirtualMachine) -> PyTupleRef {
                let iter = builtins_iter(vm).to_owned();
                let internal = self.internal.lock();
                let entries = match &internal.status {
                    IterStatus::Active(dict) => dict
                        .into_iter()
                        .map(|(key, value)| ($result_fn)(vm, key, value))
                        .collect::<Vec<_>>(),
                    IterStatus::Exhausted => vec![],
                };
                vm.new_tuple((iter, (vm.ctx.new_list(entries),)))
            }
        }

        impl Unconstructible for $iter_name {}

        impl SelfIter for $iter_name {}
        impl IterNext for $iter_name {
            #[allow(clippy::redundant_closure_call)]
            fn next(zelf: &Py<Self>, vm: &VirtualMachine) -> PyResult<PyIterReturn> {
                let mut internal = zelf.internal.lock();
                let next = if let IterStatus::Active(dict) = &internal.status {
                    if dict.entries.has_changed_size(&zelf.size) {
                        internal.status = IterStatus::Exhausted;
                        return Err(
                            vm.new_runtime_error("dictionary changed size during iteration")
                        );
                    }
                    match dict.entries.next_entry(internal.position) {
                        Some((position, key, value)) => {
                            internal.position = position;
                            PyIterReturn::Return(($result_fn)(vm, key, value))
                        }
                        None => {
                            internal.status = IterStatus::Exhausted;
                            PyIterReturn::StopIteration(None)
                        }
                    }
                } else {
                    PyIterReturn::StopIteration(None)
                };
                Ok(next)
            }
        }

        #[pyclass(module = false, name = $reverse_iter_class_name)]
        #[derive(Debug)]
        pub(crate) struct $reverse_iter_name {
            pub size: dict_inner::DictSize,
            internal: PyMutex<PositionIterInternal<PyDictRef>>,
        }

        impl PyPayload for $reverse_iter_name {
            #[inline]
            fn class(ctx: &Context) -> &'static Py<PyType> {
                ctx.types.$reverse_iter_class
            }
        }

        #[pyclass(with(Unconstructible, IterNext, Iterable))]
        impl $reverse_iter_name {
            fn new(dict: PyDictRef) -> Self {
                let size = dict.size();
                let position = size.entries_size.saturating_sub(1);
                $reverse_iter_name {
                    size,
                    internal: PyMutex::new(PositionIterInternal::new(dict, position)),
                }
            }

            #[allow(clippy::redundant_closure_call)]
            #[pymethod]
            fn __reduce__(&self, vm: &VirtualMachine) -> PyTupleRef {
                let iter = builtins_reversed(vm).to_owned();
                let internal = self.internal.lock();
                // TODO: entries must be reversed too
                let entries = match &internal.status {
                    IterStatus::Active(dict) => dict
                        .into_iter()
                        .map(|(key, value)| ($result_fn)(vm, key, value))
                        .collect::<Vec<_>>(),
                    IterStatus::Exhausted => vec![],
                };
                vm.new_tuple((iter, (vm.ctx.new_list(entries),)))
            }

            #[pymethod]
            fn __length_hint__(&self) -> usize {
                self.internal
                    .lock()
                    .rev_length_hint(|_| self.size.entries_size)
            }
        }
        impl Unconstructible for $reverse_iter_name {}

        impl SelfIter for $reverse_iter_name {}
        impl IterNext for $reverse_iter_name {
            #[allow(clippy::redundant_closure_call)]
            fn next(zelf: &Py<Self>, vm: &VirtualMachine) -> PyResult<PyIterReturn> {
                let mut internal = zelf.internal.lock();
                let next = if let IterStatus::Active(dict) = &internal.status {
                    if dict.entries.has_changed_size(&zelf.size) {
                        internal.status = IterStatus::Exhausted;
                        return Err(
                            vm.new_runtime_error("dictionary changed size during iteration")
                        );
                    }
                    match dict.entries.prev_entry(internal.position) {
                        Some((position, key, value)) => {
                            if internal.position == position {
                                internal.status = IterStatus::Exhausted;
                            } else {
                                internal.position = position;
                            }
                            PyIterReturn::Return(($result_fn)(vm, key, value))
                        }
                        None => {
                            internal.status = IterStatus::Exhausted;
                            PyIterReturn::StopIteration(None)
                        }
                    }
                } else {
                    PyIterReturn::StopIteration(None)
                };
                Ok(next)
            }
        }
    };
}

dict_view! {
    PyDictKeys,
    PyDictKeyIterator,
    PyDictReverseKeyIterator,
    dict_keys_type,
    dict_keyiterator_type,
    dict_reversekeyiterator_type,
    "dict_keys",
    "dict_keyiterator",
    "dict_reversekeyiterator",
    |_vm: &VirtualMachine, key: PyObjectRef, _value: PyObjectRef| key
}

dict_view! {
    PyDictValues,
    PyDictValueIterator,
    PyDictReverseValueIterator,
    dict_values_type,
    dict_valueiterator_type,
    dict_reversevalueiterator_type,
    "dict_values",
    "dict_valueiterator",
    "dict_reversevalueiterator",
    |_vm: &VirtualMachine, _key: PyObjectRef, value: PyObjectRef| value
}

dict_view! {
    PyDictItems,
    PyDictItemIterator,
    PyDictReverseItemIterator,
    dict_items_type,
    dict_itemiterator_type,
    dict_reverseitemiterator_type,
    "dict_items",
    "dict_itemiterator",
    "dict_reverseitemiterator",
    |vm: &VirtualMachine, key: PyObjectRef, value: PyObjectRef|
        vm.new_tuple((key, value)).into()
}

// Set operations defined on set-like views of the dictionary.
#[pyclass]
trait ViewSetOps: DictView {
    fn to_set(zelf: PyRef<Self>, vm: &VirtualMachine) -> PyResult<PySetInner> {
        let len = zelf.dict().__len__();
        let zelf: PyObjectRef = Self::iter(zelf, vm)?;
        let iter = PyIterIter::new(vm, zelf, Some(len));
        PySetInner::from_iter(iter, vm)
    }

    #[pymethod(name = "__rxor__")]
    #[pymethod]
    fn __xor__(zelf: PyRef<Self>, other: ArgIterable, vm: &VirtualMachine) -> PyResult<PySet> {
        let zelf = Self::to_set(zelf, vm)?;
        let inner = zelf.symmetric_difference(other, vm)?;
        Ok(PySet { inner })
    }

    #[pymethod(name = "__rand__")]
    #[pymethod]
    fn __and__(zelf: PyRef<Self>, other: ArgIterable, vm: &VirtualMachine) -> PyResult<PySet> {
        let zelf = Self::to_set(zelf, vm)?;
        let inner = zelf.intersection(other, vm)?;
        Ok(PySet { inner })
    }

    #[pymethod(name = "__ror__")]
    #[pymethod]
    fn __or__(zelf: PyRef<Self>, other: ArgIterable, vm: &VirtualMachine) -> PyResult<PySet> {
        let zelf = Self::to_set(zelf, vm)?;
        let inner = zelf.union(other, vm)?;
        Ok(PySet { inner })
    }

    #[pymethod]
    fn __sub__(zelf: PyRef<Self>, other: ArgIterable, vm: &VirtualMachine) -> PyResult<PySet> {
        let zelf = Self::to_set(zelf, vm)?;
        let inner = zelf.difference(other, vm)?;
        Ok(PySet { inner })
    }

    #[pymethod]
    fn __rsub__(zelf: PyRef<Self>, other: ArgIterable, vm: &VirtualMachine) -> PyResult<PySet> {
        let left = PySetInner::from_iter(other.iter(vm)?, vm)?;
        let right = ArgIterable::try_from_object(vm, Self::iter(zelf, vm)?)?;
        let inner = left.difference(right, vm)?;
        Ok(PySet { inner })
    }

    fn cmp(
        zelf: &Py<Self>,
        other: &PyObject,
        op: PyComparisonOp,
        vm: &VirtualMachine,
    ) -> PyResult<PyComparisonValue> {
        match_class!(match other {
            ref dictview @ Self => {
                return zelf.dict().inner_cmp(
                    dictview.dict(),
                    op,
                    !zelf.class().is(vm.ctx.types.dict_keys_type),
                    vm,
                );
            }
            ref _set @ PySet => {
                let inner = Self::to_set(zelf.to_owned(), vm)?;
                let zelf_set = PySet { inner }.into_pyobject(vm);
                return PySet::cmp(zelf_set.downcast_ref().unwrap(), other, op, vm);
            }
            ref _dictitems @ PyDictItems => {}
            ref _dictkeys @ PyDictKeys => {}
            _ => {
                return Ok(NotImplemented);
            }
        });
        let lhs: Vec<PyObjectRef> = zelf.as_object().to_owned().try_into_value(vm)?;
        let rhs: Vec<PyObjectRef> = other.to_owned().try_into_value(vm)?;
        lhs.iter()
            .richcompare(rhs.iter(), op, vm)
            .map(PyComparisonValue::Implemented)
    }

    #[pymethod]
    fn isdisjoint(zelf: PyRef<Self>, other: ArgIterable, vm: &VirtualMachine) -> PyResult<bool> {
        // TODO: to_set is an expensive operation. After merging #3316 rewrite implementation using PySequence_Contains.
        let zelf = Self::to_set(zelf, vm)?;
        let result = zelf.isdisjoint(other, vm)?;
        Ok(result)
    }
}

impl ViewSetOps for PyDictKeys {}
#[pyclass(with(
    DictView,
    Unconstructible,
    Comparable,
    Iterable,
    ViewSetOps,
    AsSequence,
    AsNumber,
    Representable
))]
impl PyDictKeys {
    #[pymethod]
    fn __contains__(zelf: PyObjectRef, key: PyObjectRef, vm: &VirtualMachine) -> PyResult<bool> {
        zelf.to_sequence().contains(&key, vm)
    }

    #[pygetset]
    fn mapping(zelf: PyRef<Self>) -> PyMappingProxy {
        PyMappingProxy::from(zelf.dict().clone())
    }
}
impl Unconstructible for PyDictKeys {}

impl Comparable for PyDictKeys {
    fn cmp(
        zelf: &Py<Self>,
        other: &PyObject,
        op: PyComparisonOp,
        vm: &VirtualMachine,
    ) -> PyResult<PyComparisonValue> {
        ViewSetOps::cmp(zelf, other, op, vm)
    }
}

impl AsSequence for PyDictKeys {
    fn as_sequence() -> &'static PySequenceMethods {
        static AS_SEQUENCE: LazyLock<PySequenceMethods> = LazyLock::new(|| PySequenceMethods {
            length: atomic_func!(|seq, _vm| Ok(PyDictKeys::sequence_downcast(seq).__len__())),
            contains: atomic_func!(|seq, target, vm| {
                PyDictKeys::sequence_downcast(seq)
                    .dict
                    .entries
                    .contains(vm, target)
            }),
            ..PySequenceMethods::NOT_IMPLEMENTED
        });
        &AS_SEQUENCE
    }
}

impl AsNumber for PyDictKeys {
    fn as_number() -> &'static PyNumberMethods {
        static AS_NUMBER: PyNumberMethods = PyNumberMethods {
            subtract: Some(set_inner_number_subtract),
            and: Some(set_inner_number_and),
            xor: Some(set_inner_number_xor),
            or: Some(set_inner_number_or),
            ..PyNumberMethods::NOT_IMPLEMENTED
        };
        &AS_NUMBER
    }
}

impl ViewSetOps for PyDictItems {}
#[pyclass(with(
    DictView,
    Unconstructible,
    Comparable,
    Iterable,
    ViewSetOps,
    AsSequence,
    AsNumber,
    Representable
))]
impl PyDictItems {
    #[pymethod]
    fn __contains__(zelf: PyObjectRef, needle: PyObjectRef, vm: &VirtualMachine) -> PyResult<bool> {
        zelf.to_sequence().contains(&needle, vm)
    }
    #[pygetset]
    fn mapping(zelf: PyRef<Self>) -> PyMappingProxy {
        PyMappingProxy::from(zelf.dict().clone())
    }
}
impl Unconstructible for PyDictItems {}

impl Comparable for PyDictItems {
    fn cmp(
        zelf: &Py<Self>,
        other: &PyObject,
        op: PyComparisonOp,
        vm: &VirtualMachine,
    ) -> PyResult<PyComparisonValue> {
        ViewSetOps::cmp(zelf, other, op, vm)
    }
}

impl AsSequence for PyDictItems {
    fn as_sequence() -> &'static PySequenceMethods {
        static AS_SEQUENCE: LazyLock<PySequenceMethods> = LazyLock::new(|| PySequenceMethods {
            length: atomic_func!(|seq, _vm| Ok(PyDictItems::sequence_downcast(seq).__len__())),
            contains: atomic_func!(|seq, target, vm| {
                let needle: &Py<PyTuple> = match target.downcast_ref() {
                    Some(needle) => needle,
                    None => return Ok(false),
                };
                if needle.len() != 2 {
                    return Ok(false);
                }

                let zelf = PyDictItems::sequence_downcast(seq);
                let key = &needle[0];
                if !zelf.dict.__contains__(key.to_owned(), vm)? {
                    return Ok(false);
                }
                let value = &needle[1];
                let found = zelf.dict().__getitem__(key.to_owned(), vm)?;
                vm.identical_or_equal(&found, value)
            }),
            ..PySequenceMethods::NOT_IMPLEMENTED
        });
        &AS_SEQUENCE
    }
}

impl AsNumber for PyDictItems {
    fn as_number() -> &'static PyNumberMethods {
        static AS_NUMBER: PyNumberMethods = PyNumberMethods {
            subtract: Some(set_inner_number_subtract),
            and: Some(set_inner_number_and),
            xor: Some(set_inner_number_xor),
            or: Some(set_inner_number_or),
            ..PyNumberMethods::NOT_IMPLEMENTED
        };
        &AS_NUMBER
    }
}

#[pyclass(with(DictView, Unconstructible, Iterable, AsSequence, Representable))]
impl PyDictValues {
    #[pygetset]
    fn mapping(zelf: PyRef<Self>) -> PyMappingProxy {
        PyMappingProxy::from(zelf.dict().clone())
    }
}
impl Unconstructible for PyDictValues {}

impl AsSequence for PyDictValues {
    fn as_sequence() -> &'static PySequenceMethods {
        static AS_SEQUENCE: LazyLock<PySequenceMethods> = LazyLock::new(|| PySequenceMethods {
            length: atomic_func!(|seq, _vm| Ok(PyDictValues::sequence_downcast(seq).__len__())),
            ..PySequenceMethods::NOT_IMPLEMENTED
        });
        &AS_SEQUENCE
    }
}

fn set_inner_number_op<F>(a: &PyObject, b: &PyObject, f: F, vm: &VirtualMachine) -> PyResult
where
    F: FnOnce(PySetInner, ArgIterable) -> PyResult<PySetInner>,
{
    let a = PySetInner::from_iter(
        ArgIterable::try_from_object(vm, a.to_owned())?.iter(vm)?,
        vm,
    )?;
    let b = ArgIterable::try_from_object(vm, b.to_owned())?;
    Ok(PySet { inner: f(a, b)? }.into_pyobject(vm))
}

fn set_inner_number_subtract(a: &PyObject, b: &PyObject, vm: &VirtualMachine) -> PyResult {
    set_inner_number_op(a, b, |a, b| a.difference(b, vm), vm)
}

fn set_inner_number_and(a: &PyObject, b: &PyObject, vm: &VirtualMachine) -> PyResult {
    set_inner_number_op(a, b, |a, b| a.intersection(b, vm), vm)
}

fn set_inner_number_xor(a: &PyObject, b: &PyObject, vm: &VirtualMachine) -> PyResult {
    set_inner_number_op(a, b, |a, b| a.symmetric_difference(b, vm), vm)
}

fn set_inner_number_or(a: &PyObject, b: &PyObject, vm: &VirtualMachine) -> PyResult {
    set_inner_number_op(a, b, |a, b| a.union(b, vm), vm)
}

pub(crate) fn init(context: &Context) {
    PyDict::extend_class(context, context.types.dict_type);
    PyDictKeys::extend_class(context, context.types.dict_keys_type);
    PyDictKeyIterator::extend_class(context, context.types.dict_keyiterator_type);
    PyDictReverseKeyIterator::extend_class(context, context.types.dict_reversekeyiterator_type);
    PyDictValues::extend_class(context, context.types.dict_values_type);
    PyDictValueIterator::extend_class(context, context.types.dict_valueiterator_type);
    PyDictReverseValueIterator::extend_class(context, context.types.dict_reversevalueiterator_type);
    PyDictItems::extend_class(context, context.types.dict_items_type);
    PyDictItemIterator::extend_class(context, context.types.dict_itemiterator_type);
    PyDictReverseItemIterator::extend_class(context, context.types.dict_reverseitemiterator_type);
}
