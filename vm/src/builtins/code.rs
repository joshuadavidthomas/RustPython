/*! Infamous code object. The python class `code`

*/

use super::{PyStrRef, PyTupleRef, PyType, PyTypeRef};
use crate::{
    AsObject, Context, Py, PyObject, PyObjectRef, PyPayload, PyResult, VirtualMachine,
    builtins::PyStrInterned,
    bytecode::{self, AsBag, BorrowedConstant, CodeFlags, Constant, ConstantBag},
    class::{PyClassImpl, StaticType},
    convert::ToPyObject,
    frozen,
    function::{FuncArgs, OptionalArg},
    types::Representable,
};
use malachite_bigint::BigInt;
use num_traits::Zero;
use rustpython_compiler_core::OneIndexed;
use std::{borrow::Borrow, fmt, ops::Deref};

#[derive(FromArgs)]
pub struct ReplaceArgs {
    #[pyarg(named, optional)]
    co_posonlyargcount: OptionalArg<u32>,
    #[pyarg(named, optional)]
    co_argcount: OptionalArg<u32>,
    #[pyarg(named, optional)]
    co_kwonlyargcount: OptionalArg<u32>,
    #[pyarg(named, optional)]
    co_filename: OptionalArg<PyStrRef>,
    #[pyarg(named, optional)]
    co_firstlineno: OptionalArg<u32>,
    #[pyarg(named, optional)]
    co_consts: OptionalArg<Vec<PyObjectRef>>,
    #[pyarg(named, optional)]
    co_name: OptionalArg<PyStrRef>,
    #[pyarg(named, optional)]
    co_names: OptionalArg<Vec<PyObjectRef>>,
    #[pyarg(named, optional)]
    co_flags: OptionalArg<u16>,
    #[pyarg(named, optional)]
    co_varnames: OptionalArg<Vec<PyObjectRef>>,
}

#[derive(Clone)]
#[repr(transparent)]
pub struct Literal(PyObjectRef);

impl Borrow<PyObject> for Literal {
    fn borrow(&self) -> &PyObject {
        &self.0
    }
}

impl From<Literal> for PyObjectRef {
    fn from(obj: Literal) -> Self {
        obj.0
    }
}

fn borrow_obj_constant(obj: &PyObject) -> BorrowedConstant<'_, Literal> {
    match_class!(match obj {
        ref i @ super::int::PyInt => {
            let value = i.as_bigint();
            if obj.class().is(super::bool_::PyBool::static_type()) {
                BorrowedConstant::Boolean {
                    value: !value.is_zero(),
                }
            } else {
                BorrowedConstant::Integer { value }
            }
        }
        ref f @ super::float::PyFloat => BorrowedConstant::Float { value: f.to_f64() },
        ref c @ super::complex::PyComplex => BorrowedConstant::Complex {
            value: c.to_complex()
        },
        ref s @ super::pystr::PyStr => BorrowedConstant::Str { value: s.as_wtf8() },
        ref b @ super::bytes::PyBytes => BorrowedConstant::Bytes {
            value: b.as_bytes()
        },
        ref c @ PyCode => {
            BorrowedConstant::Code { code: &c.code }
        }
        ref t @ super::tuple::PyTuple => {
            let elements = t.as_slice();
            // SAFETY: Literal is repr(transparent) over PyObjectRef, and a Literal tuple only ever
            //         has other literals as elements
            let elements = unsafe { &*(elements as *const [PyObjectRef] as *const [Literal]) };
            BorrowedConstant::Tuple { elements }
        }
        super::singletons::PyNone => BorrowedConstant::None,
        super::slice::PyEllipsis => BorrowedConstant::Ellipsis,
        _ => panic!("unexpected payload for constant python value"),
    })
}

impl Constant for Literal {
    type Name = &'static PyStrInterned;
    fn borrow_constant(&self) -> BorrowedConstant<'_, Self> {
        borrow_obj_constant(&self.0)
    }
}

impl<'a> AsBag for &'a Context {
    type Bag = PyObjBag<'a>;
    fn as_bag(self) -> PyObjBag<'a> {
        PyObjBag(self)
    }
}

impl<'a> AsBag for &'a VirtualMachine {
    type Bag = PyObjBag<'a>;
    fn as_bag(self) -> PyObjBag<'a> {
        PyObjBag(&self.ctx)
    }
}

#[derive(Clone, Copy)]
pub struct PyObjBag<'a>(pub &'a Context);

impl ConstantBag for PyObjBag<'_> {
    type Constant = Literal;

    fn make_constant<C: Constant>(&self, constant: BorrowedConstant<'_, C>) -> Self::Constant {
        let ctx = self.0;
        let obj = match constant {
            BorrowedConstant::Integer { value } => ctx.new_bigint(value).into(),
            BorrowedConstant::Float { value } => ctx.new_float(value).into(),
            BorrowedConstant::Complex { value } => ctx.new_complex(value).into(),
            BorrowedConstant::Str { value } if value.len() <= 20 => {
                ctx.intern_str(value).to_object()
            }
            BorrowedConstant::Str { value } => ctx.new_str(value).into(),
            BorrowedConstant::Bytes { value } => ctx.new_bytes(value.to_vec()).into(),
            BorrowedConstant::Boolean { value } => ctx.new_bool(value).into(),
            BorrowedConstant::Code { code } => ctx.new_code(code.map_clone_bag(self)).into(),
            BorrowedConstant::Tuple { elements } => {
                let elements = elements
                    .iter()
                    .map(|constant| self.make_constant(constant.borrow_constant()).0)
                    .collect();
                ctx.new_tuple(elements).into()
            }
            BorrowedConstant::None => ctx.none(),
            BorrowedConstant::Ellipsis => ctx.ellipsis.clone().into(),
        };

        Literal(obj)
    }

    fn make_name(&self, name: &str) -> &'static PyStrInterned {
        self.0.intern_str(name)
    }

    fn make_int(&self, value: BigInt) -> Self::Constant {
        Literal(self.0.new_int(value).into())
    }

    fn make_tuple(&self, elements: impl Iterator<Item = Self::Constant>) -> Self::Constant {
        Literal(self.0.new_tuple(elements.map(|lit| lit.0).collect()).into())
    }

    fn make_code(&self, code: CodeObject) -> Self::Constant {
        Literal(self.0.new_code(code).into())
    }
}

pub type CodeObject = bytecode::CodeObject<Literal>;

pub trait IntoCodeObject {
    fn into_code_object(self, ctx: &Context) -> CodeObject;
}

impl IntoCodeObject for CodeObject {
    fn into_code_object(self, _ctx: &Context) -> Self {
        self
    }
}

impl IntoCodeObject for bytecode::CodeObject {
    fn into_code_object(self, ctx: &Context) -> CodeObject {
        self.map_bag(PyObjBag(ctx))
    }
}

impl<B: AsRef<[u8]>> IntoCodeObject for frozen::FrozenCodeObject<B> {
    fn into_code_object(self, ctx: &Context) -> CodeObject {
        self.decode(ctx)
    }
}

#[pyclass(module = false, name = "code")]
pub struct PyCode {
    pub code: CodeObject,
}

impl Deref for PyCode {
    type Target = CodeObject;
    fn deref(&self) -> &Self::Target {
        &self.code
    }
}

impl PyCode {
    pub const fn new(code: CodeObject) -> Self {
        Self { code }
    }
}

impl fmt::Debug for PyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "code: {:?}", self.code)
    }
}

impl PyPayload for PyCode {
    #[inline]
    fn class(ctx: &Context) -> &'static Py<PyType> {
        ctx.types.code_type
    }
}

impl Representable for PyCode {
    #[inline]
    fn repr_str(zelf: &Py<Self>, _vm: &VirtualMachine) -> PyResult<String> {
        let code = &zelf.code;
        Ok(format!(
            "<code object {} at {:#x} file {:?}, line {}>",
            code.obj_name,
            zelf.get_id(),
            code.source_path.as_str(),
            code.first_line_number.map_or(-1, |n| n.get() as i32)
        ))
    }
}

#[pyclass(with(Representable))]
impl PyCode {
    #[pyslot]
    fn slot_new(_cls: PyTypeRef, _args: FuncArgs, vm: &VirtualMachine) -> PyResult {
        Err(vm.new_type_error("Cannot directly create code object"))
    }

    #[pygetset]
    const fn co_posonlyargcount(&self) -> usize {
        self.code.posonlyarg_count as usize
    }

    #[pygetset]
    const fn co_argcount(&self) -> usize {
        self.code.arg_count as usize
    }

    #[pygetset]
    const fn co_stacksize(&self) -> u32 {
        self.code.max_stackdepth
    }

    #[pygetset]
    pub fn co_filename(&self) -> PyStrRef {
        self.code.source_path.to_owned()
    }

    #[pygetset]
    pub fn co_cellvars(&self, vm: &VirtualMachine) -> PyTupleRef {
        let cellvars = self
            .code
            .cellvars
            .deref()
            .iter()
            .map(|name| name.to_pyobject(vm))
            .collect();
        vm.ctx.new_tuple(cellvars)
    }

    #[pygetset]
    fn co_nlocals(&self) -> usize {
        self.varnames.len()
    }

    #[pygetset]
    fn co_firstlineno(&self) -> u32 {
        self.code.first_line_number.map_or(0, |n| n.get() as _)
    }

    #[pygetset]
    const fn co_kwonlyargcount(&self) -> usize {
        self.code.kwonlyarg_count as usize
    }

    #[pygetset]
    fn co_consts(&self, vm: &VirtualMachine) -> PyTupleRef {
        let consts = self.code.constants.iter().map(|x| x.0.clone()).collect();
        vm.ctx.new_tuple(consts)
    }

    #[pygetset]
    fn co_name(&self) -> PyStrRef {
        self.code.obj_name.to_owned()
    }
    #[pygetset]
    fn co_qualname(&self) -> PyStrRef {
        self.code.qualname.to_owned()
    }

    #[pygetset]
    fn co_names(&self, vm: &VirtualMachine) -> PyTupleRef {
        let names = self
            .code
            .names
            .deref()
            .iter()
            .map(|name| name.to_pyobject(vm))
            .collect();
        vm.ctx.new_tuple(names)
    }

    #[pygetset]
    const fn co_flags(&self) -> u16 {
        self.code.flags.bits()
    }

    #[pygetset]
    pub fn co_varnames(&self, vm: &VirtualMachine) -> PyTupleRef {
        let varnames = self.code.varnames.iter().map(|s| s.to_object()).collect();
        vm.ctx.new_tuple(varnames)
    }

    #[pygetset]
    pub fn co_code(&self, vm: &VirtualMachine) -> crate::builtins::PyBytesRef {
        // SAFETY: CodeUnit is #[repr(C)] with size 2, so we can safely transmute to bytes
        let bytes = unsafe {
            std::slice::from_raw_parts(
                self.code.instructions.as_ptr() as *const u8,
                self.code.instructions.len() * 2,
            )
        };
        vm.ctx.new_bytes(bytes.to_vec())
    }

    #[pygetset]
    pub fn co_freevars(&self, vm: &VirtualMachine) -> PyTupleRef {
        let names = self
            .code
            .freevars
            .deref()
            .iter()
            .map(|name| name.to_pyobject(vm))
            .collect();
        vm.ctx.new_tuple(names)
    }

    #[pymethod]
    pub fn replace(&self, args: ReplaceArgs, vm: &VirtualMachine) -> PyResult<Self> {
        let posonlyarg_count = match args.co_posonlyargcount {
            OptionalArg::Present(posonlyarg_count) => posonlyarg_count,
            OptionalArg::Missing => self.code.posonlyarg_count,
        };

        let arg_count = match args.co_argcount {
            OptionalArg::Present(arg_count) => arg_count,
            OptionalArg::Missing => self.code.arg_count,
        };

        let source_path = match args.co_filename {
            OptionalArg::Present(source_path) => source_path,
            OptionalArg::Missing => self.code.source_path.to_owned(),
        };

        let first_line_number = match args.co_firstlineno {
            OptionalArg::Present(first_line_number) => OneIndexed::new(first_line_number as _),
            OptionalArg::Missing => self.code.first_line_number,
        };

        let kwonlyarg_count = match args.co_kwonlyargcount {
            OptionalArg::Present(kwonlyarg_count) => kwonlyarg_count,
            OptionalArg::Missing => self.code.kwonlyarg_count,
        };

        let constants = match args.co_consts {
            OptionalArg::Present(constants) => constants,
            OptionalArg::Missing => self.code.constants.iter().map(|x| x.0.clone()).collect(),
        };

        let obj_name = match args.co_name {
            OptionalArg::Present(obj_name) => obj_name,
            OptionalArg::Missing => self.code.obj_name.to_owned(),
        };

        let names = match args.co_names {
            OptionalArg::Present(names) => names,
            OptionalArg::Missing => self
                .code
                .names
                .deref()
                .iter()
                .map(|name| name.to_pyobject(vm))
                .collect(),
        };

        let flags = match args.co_flags {
            OptionalArg::Present(flags) => flags,
            OptionalArg::Missing => self.code.flags.bits(),
        };

        let varnames = match args.co_varnames {
            OptionalArg::Present(varnames) => varnames,
            OptionalArg::Missing => self.code.varnames.iter().map(|s| s.to_object()).collect(),
        };

        Ok(Self {
            code: CodeObject {
                flags: CodeFlags::from_bits_truncate(flags),
                posonlyarg_count,
                arg_count,
                kwonlyarg_count,
                source_path: source_path.as_object().as_interned_str(vm).unwrap(),
                first_line_number,
                obj_name: obj_name.as_object().as_interned_str(vm).unwrap(),
                qualname: self.code.qualname,

                max_stackdepth: self.code.max_stackdepth,
                instructions: self.code.instructions.clone(),
                locations: self.code.locations.clone(),
                constants: constants.into_iter().map(Literal).collect(),
                names: names
                    .into_iter()
                    .map(|o| o.as_interned_str(vm).unwrap())
                    .collect(),
                varnames: varnames
                    .into_iter()
                    .map(|o| o.as_interned_str(vm).unwrap())
                    .collect(),
                cellvars: self.code.cellvars.clone(),
                freevars: self.code.freevars.clone(),
                cell2arg: self.code.cell2arg.clone(),
            },
        })
    }
}

impl fmt::Display for PyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl ToPyObject for CodeObject {
    fn to_pyobject(self, vm: &VirtualMachine) -> PyObjectRef {
        vm.ctx.new_code(self).into()
    }
}

impl ToPyObject for bytecode::CodeObject {
    fn to_pyobject(self, vm: &VirtualMachine) -> PyObjectRef {
        vm.ctx.new_code(self).into()
    }
}

pub fn init(ctx: &Context) {
    PyCode::extend_class(ctx, ctx.types.code_type);
}
