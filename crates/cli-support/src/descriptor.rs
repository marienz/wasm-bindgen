use std::char;

macro_rules! tys {
    ($($a:ident)*) => (tys! { @ ($($a)*) 0 });
    (@ () $v:expr) => {};
    (@ ($a:ident $($b:ident)*) $v:expr) => {
        const $a: u32 = $v;
        tys!(@ ($($b)*) $v+1);
    }
}

// NB: this list must be kept in sync with `src/describe.rs`
tys! {
    I8
    U8
    I16
    U16
    I32
    U32
    I64
    U64
    F32
    F64
    BOOLEAN
    FUNCTION
    CLOSURE
    STRING
    REF
    REFMUT
    SLICE
    VECTOR
    ANYREF
    ENUM
    RUST_STRUCT
    CHAR
    OPTIONAL
    UNIT
    CLAMPED
}

#[derive(Debug, Clone)]
pub enum Descriptor {
    I8,
    U8,
    ClampedU8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
    Boolean,
    Function(Box<Function>),
    Closure(Box<Closure>),
    Ref(Box<Descriptor>),
    RefMut(Box<Descriptor>),
    Slice(Box<Descriptor>),
    Vector(Box<Descriptor>),
    String,
    Anyref,
    Enum { hole: u32 },
    RustStruct(String),
    Char,
    Option(Box<Descriptor>),
    Unit,
}

#[derive(Debug, Clone)]
pub struct Function {
    pub arguments: Vec<Descriptor>,
    pub shim_idx: u32,
    pub ret: Descriptor,
}

#[derive(Debug, Clone)]
pub struct Closure {
    pub shim_idx: u32,
    pub dtor_idx: u32,
    pub function: Function,
    pub mutable: bool,
}

#[derive(Copy, Clone)]
pub enum VectorKind {
    I8,
    U8,
    ClampedU8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
    String,
    Anyref,
}

pub struct Number {
    u32: bool,
}

impl Descriptor {
    pub fn decode(mut data: &[u32]) -> Descriptor {
        let descriptor = Descriptor::_decode(&mut data, false);
        assert!(data.is_empty(), "remaining data {:?}", data);
        descriptor
    }

    fn _decode(data: &mut &[u32], clamped: bool) -> Descriptor {
        match get(data) {
            I8 => Descriptor::I8,
            I16 => Descriptor::I16,
            I32 => Descriptor::I32,
            I64 => Descriptor::I64,
            U8 if clamped => Descriptor::ClampedU8,
            U8 => Descriptor::U8,
            U16 => Descriptor::U16,
            U32 => Descriptor::U32,
            U64 => Descriptor::U64,
            F32 => Descriptor::F32,
            F64 => Descriptor::F64,
            BOOLEAN => Descriptor::Boolean,
            FUNCTION => Descriptor::Function(Box::new(Function::decode(data))),
            CLOSURE => Descriptor::Closure(Box::new(Closure::decode(data))),
            REF => Descriptor::Ref(Box::new(Descriptor::_decode(data, clamped))),
            REFMUT => Descriptor::RefMut(Box::new(Descriptor::_decode(data, clamped))),
            SLICE => Descriptor::Slice(Box::new(Descriptor::_decode(data, clamped))),
            VECTOR => Descriptor::Vector(Box::new(Descriptor::_decode(data, clamped))),
            OPTIONAL => Descriptor::Option(Box::new(Descriptor::_decode(data, clamped))),
            STRING => Descriptor::String,
            ANYREF => Descriptor::Anyref,
            ENUM => Descriptor::Enum { hole: get(data) },
            RUST_STRUCT => {
                let name = (0..get(data))
                    .map(|_| char::from_u32(get(data)).unwrap())
                    .collect();
                Descriptor::RustStruct(name)
            }
            CHAR => Descriptor::Char,
            UNIT => Descriptor::Unit,
            CLAMPED => Descriptor::_decode(data, true),
            other => panic!("unknown descriptor: {}", other),
        }
    }

    pub fn unwrap_function(self) -> Function {
        match self {
            Descriptor::Function(f) => *f,
            _ => panic!("not a function"),
        }
    }

    /// Returns `Some` if this type is a number, and the returned `Number` type
    /// can be accessed to learn more about what kind of number this is.
    pub fn number(&self) -> Option<Number> {
        match *self {
            Descriptor::I8
            | Descriptor::U8
            | Descriptor::I16
            | Descriptor::U16
            | Descriptor::I32
            | Descriptor::F32
            | Descriptor::F64
            | Descriptor::Enum { .. } => Some(Number { u32: false }),
            Descriptor::U32 => Some(Number { u32: true }),
            _ => None,
        }
    }

    pub fn is_wasm_native(&self) -> bool {
        match *self {
            Descriptor::I32 | Descriptor::U32 | Descriptor::F32 | Descriptor::F64 => true,
            _ => return false,
        }
    }

    pub fn is_abi_as_u32(&self) -> bool {
        match *self {
            Descriptor::I8 | Descriptor::U8 | Descriptor::I16 | Descriptor::U16 => true,
            _ => return false,
        }
    }

    pub fn get_64(&self) -> Option<bool> {
        match *self {
            Descriptor::I64 => Some(true),
            Descriptor::U64 => Some(false),
            _ => None,
        }
    }

    pub fn is_ref_anyref(&self) -> bool {
        match *self {
            Descriptor::Ref(ref s) => s.is_anyref(),
            _ => return false,
        }
    }

    pub fn unwrap_closure(self) -> Closure {
        match self {
            Descriptor::Closure(s) => *s,
            _ => panic!("not a closure"),
        }
    }

    pub fn is_anyref(&self) -> bool {
        match *self {
            Descriptor::Anyref => true,
            _ => false,
        }
    }

    pub fn vector_kind(&self) -> Option<VectorKind> {
        let inner = match *self {
            Descriptor::String => return Some(VectorKind::String),
            Descriptor::Vector(ref d) => &**d,
            Descriptor::Slice(ref d) => &**d,
            Descriptor::Ref(ref d) => match **d {
                Descriptor::Slice(ref d) => &**d,
                Descriptor::String => return Some(VectorKind::String),
                _ => return None,
            },
            Descriptor::RefMut(ref d) => match **d {
                Descriptor::Slice(ref d) => &**d,
                _ => return None,
            },
            _ => return None,
        };
        match *inner {
            Descriptor::I8 => Some(VectorKind::I8),
            Descriptor::I16 => Some(VectorKind::I16),
            Descriptor::I32 => Some(VectorKind::I32),
            Descriptor::I64 => Some(VectorKind::I64),
            Descriptor::U8 => Some(VectorKind::U8),
            Descriptor::ClampedU8 => Some(VectorKind::ClampedU8),
            Descriptor::U16 => Some(VectorKind::U16),
            Descriptor::U32 => Some(VectorKind::U32),
            Descriptor::U64 => Some(VectorKind::U64),
            Descriptor::F32 => Some(VectorKind::F32),
            Descriptor::F64 => Some(VectorKind::F64),
            Descriptor::Anyref => Some(VectorKind::Anyref),
            _ => None,
        }
    }

    pub fn rust_struct(&self) -> Option<&str> {
        let inner = match *self {
            Descriptor::Ref(ref d) => &**d,
            Descriptor::RefMut(ref d) => &**d,
            ref d => d,
        };
        match *inner {
            Descriptor::RustStruct(ref s) => Some(s),
            _ => None,
        }
    }

    pub fn stack_closure(&self) -> Option<(&Function, bool)> {
        let (inner, mutable) = match *self {
            Descriptor::Ref(ref d) => (&**d, false),
            Descriptor::RefMut(ref d) => (&**d, true),
            _ => return None,
        };
        match *inner {
            Descriptor::Function(ref f) => Some((f, mutable)),
            _ => None,
        }
    }

    pub fn is_by_ref(&self) -> bool {
        match *self {
            Descriptor::Ref(_) | Descriptor::RefMut(_) => true,
            _ => false,
        }
    }

    pub fn is_mut_ref(&self) -> bool {
        match *self {
            Descriptor::RefMut(_) => true,
            _ => false,
        }
    }

    pub fn abi_returned_through_pointer(&self) -> bool {
        if self.vector_kind().is_some() {
            return true;
        }
        if self.get_64().is_some() {
            return true;
        }
        match self {
            Descriptor::Option(inner) => match &**inner {
                Descriptor::Anyref
                | Descriptor::RustStruct(_)
                | Descriptor::Enum { .. }
                | Descriptor::Char
                | Descriptor::Boolean
                | Descriptor::I8
                | Descriptor::U8
                | Descriptor::I16
                | Descriptor::U16 => false,
                _ => true,
            },
            _ => false,
        }
    }

    pub fn abi_arg_count(&self) -> usize {
        if let Descriptor::Option(inner) = self {
            if inner.get_64().is_some() {
                return 4;
            }
            if let Descriptor::Ref(inner) = &**inner {
                match &**inner {
                    Descriptor::Anyref => return 1,
                    _ => {}
                }
            }
        }
        if self.stack_closure().is_some() {
            return 2;
        }
        if self.abi_returned_through_pointer() {
            2
        } else {
            1
        }
    }

    pub fn assert_abi_return_correct(&self, before: usize, after: usize) {
        if before != after {
            assert_eq!(
                before + 1,
                after,
                "abi_returned_through_pointer wrong for {:?}",
                self,
            );
            assert!(
                self.abi_returned_through_pointer(),
                "abi_returned_through_pointer wrong for {:?}",
                self,
            );
        } else {
            assert!(
                !self.abi_returned_through_pointer(),
                "abi_returned_through_pointer wrong for {:?}",
                self,
            );
        }
    }

    pub fn assert_abi_arg_correct(&self, before: usize, after: usize) {
        assert_eq!(
            before + self.abi_arg_count(),
            after,
            "abi_arg_count wrong for {:?}",
            self,
        );
    }
}

fn get(a: &mut &[u32]) -> u32 {
    let ret = a[0];
    *a = &a[1..];
    ret
}

impl Closure {
    fn decode(data: &mut &[u32]) -> Closure {
        let shim_idx = get(data);
        let dtor_idx = get(data);
        let mutable = get(data) == REFMUT;
        assert_eq!(get(data), FUNCTION);
        Closure {
            shim_idx,
            dtor_idx,
            mutable,
            function: Function::decode(data),
        }
    }
}

impl Function {
    fn decode(data: &mut &[u32]) -> Function {
        let shim_idx = get(data);
        let arguments = (0..get(data))
            .map(|_| Descriptor::_decode(data, false))
            .collect::<Vec<_>>();
        Function {
            arguments,
            shim_idx,
            ret: Descriptor::_decode(data, false),
        }
    }
}

impl VectorKind {
    pub fn js_ty(&self) -> &str {
        match *self {
            VectorKind::String => "string",
            VectorKind::I8 => "Int8Array",
            VectorKind::U8 => "Uint8Array",
            VectorKind::ClampedU8 => "Uint8ClampedArray",
            VectorKind::I16 => "Int16Array",
            VectorKind::U16 => "Uint16Array",
            VectorKind::I32 => "Int32Array",
            VectorKind::U32 => "Uint32Array",
            VectorKind::I64 => "BigInt64Array",
            VectorKind::U64 => "BigUint64Array",
            VectorKind::F32 => "Float32Array",
            VectorKind::F64 => "Float64Array",
            VectorKind::Anyref => "any[]",
        }
    }

    pub fn size(&self) -> usize {
        match *self {
            VectorKind::String => 1,
            VectorKind::I8 => 1,
            VectorKind::U8 => 1,
            VectorKind::ClampedU8 => 1,
            VectorKind::I16 => 2,
            VectorKind::U16 => 2,
            VectorKind::I32 => 4,
            VectorKind::U32 => 4,
            VectorKind::I64 => 8,
            VectorKind::U64 => 8,
            VectorKind::F32 => 4,
            VectorKind::F64 => 8,
            VectorKind::Anyref => 4,
        }
    }
}

impl Number {
    pub fn is_u32(&self) -> bool {
        self.u32
    }
}
