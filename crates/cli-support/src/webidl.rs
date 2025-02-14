//! The polyfill for the WebIDL bindings proposal in wasm-bindgen.
//!
//! This module contains the polyfill (or at least the current state of and as
//! closely as we can match) the WebIDL bindings proposal. The module exports
//! one main function, `process`, which takes a `walrus::Module`. This module is
//! expected to have two items:
//!
//! * First it contains all of the raw wasm-bindgen modules emitted by the Rust
//!   compiler. These raw custom sections are extracted, removed, decoded, and
//!   handled here. They contain information such as what's exported where,
//!   what's imported, comments, etc.
//! * Second, the `descriptors.rs` pass must have run previously to execute all
//!   the descriptor functions in the wasm module. Through the synthesized
//!   custom section there we learn the type information of all
//!   functions/imports/exports in the module.
//!
//! The output of this function is then a new `walrus::Module` with the previous
//! custom sections all removed and two new ones inserted. One is the webidl
//! bindings custom section (or at least a close approximate) and the second is
//! an auxiliary section for wasm-bindgen itself. The goal is for this auxiliary
//! section to eventually be empty or inconsequential, allowing us to emit
//! something that doesn't even need a JS shim one day. For now we're still
//! pretty far away from that, so we'll settle for using webidl bindings as
//! aggressively as possible!

use crate::decode;
use crate::descriptor::{Closure, Descriptor, Function};
use crate::descriptors::WasmBindgenDescriptorsSection;
use crate::intrinsic::Intrinsic;
use failure::{bail, Error};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str;
use walrus::{ExportId, FunctionId, ImportId, Module, TypedCustomSectionId};
use wasm_bindgen_shared::struct_function_export_name;

const PLACEHOLDER_MODULE: &str = "__wbindgen_placeholder__";

/// A "dummy" WebIDL custom section. This should be replaced with a true
/// polyfill for the WebIDL bindings proposal.
#[derive(Default, Debug)]
pub struct WebidlCustomSection {
    /// A map from exported function id to the expected signature of the
    /// interface.
    ///
    /// The expected signature will contain rich types like strings/js
    /// values/etc. A WebIDL binding will be needed to ensure the JS export of
    /// the wasm mdoule either has this expected signature or a shim will need
    /// to get generated to ensure the right signature in JS is respected.
    pub exports: HashMap<ExportId, Function>,

    /// A map from imported function id to the expected binding of the
    /// interface.
    ///
    /// This will directly translate to WebIDL bindings and how it's expected
    /// that each import is invoked. Note that this also affects the polyfill
    /// glue generated.
    pub imports: HashMap<ImportId, ImportBinding>,
}

pub type WebidlCustomSectionId = TypedCustomSectionId<WebidlCustomSection>;

/// The types of functionality that can be imported and listed for each import
/// in a wasm module.
#[derive(Debug, Clone)]
pub enum ImportBinding {
    /// The imported function is considered to be a constructor, and will be
    /// invoked as if it has `new` in JS. The returned value is expected
    /// to be `anyref`.
    Constructor(Function),
    /// The imported function is considered to be a function that's called like
    /// a method in JS where the first argument should be `anyref` and it is
    /// passed as the `this` of the call.
    Method(Function),
    /// Just a bland normal import which represents some sort of function to
    /// call, not much fancy going on here.
    Function(Function),
}

/// A synthetic custom section which is not standardized, never will be, and
/// cannot be serialized or parsed. This is synthesized from all of the
/// compiler-emitted wasm-bindgen sections and then immediately removed to be
/// processed in the JS generation pass.
#[derive(Default, Debug)]
pub struct WasmBindgenAux {
    /// Extra typescript annotations that should be appended to the generated
    /// TypeScript file. This is provided via a custom attribute in Rust code.
    pub extra_typescript: String,

    /// A map from identifier to the contents of each local module defined via
    /// the `#[wasm_bindgen(module = "/foo.js")]` import options.
    pub local_modules: HashMap<String, String>,

    /// A map from unique crate identifier to the list of inline JS snippets for
    /// that crate identifier.
    pub snippets: HashMap<String, Vec<String>>,

    /// A list of all `package.json` files that are intended to be included in
    /// the final build.
    pub package_jsons: HashSet<PathBuf>,

    /// A map from exported function id to where it's expected to be exported
    /// to.
    pub export_map: HashMap<ExportId, AuxExport>,

    /// A map from imported function id to what it's expected to import.
    pub import_map: HashMap<ImportId, AuxImport>,

    /// Small bits of metadata about imports.
    pub imports_with_catch: HashSet<ImportId>,
    pub imports_with_variadic: HashSet<ImportId>,

    /// Auxiliary information to go into JS/TypeScript bindings describing the
    /// exported enums from Rust.
    pub enums: Vec<AuxEnum>,

    /// Auxiliary information to go into JS/TypeScript bindings describing the
    /// exported structs from Rust and their fields they've got exported.
    pub structs: Vec<AuxStruct>,
}

pub type WasmBindgenAuxId = TypedCustomSectionId<WasmBindgenAux>;

#[derive(Debug)]
pub struct AuxExport {
    /// When generating errors about this export, a helpful name to remember it
    /// by.
    pub debug_name: String,
    /// Comments parsed in Rust and forwarded here to show up in JS bindings.
    pub comments: String,
    /// Argument names in Rust forwarded here to configure the names that show
    /// up in TypeScript bindings.
    pub arg_names: Option<Vec<String>>,
    /// What kind of function this is and where it shows up
    pub kind: AuxExportKind,
}

/// All possible kinds of exports from a wasm module.
///
/// This `enum` says where to place an exported wasm function. For example it
/// may want to get hooked up to a JS class, or it may want to be exported as a
/// free function (etc).
///
/// TODO: it feels like this should not really be here per se. We probably want
/// to either construct the JS object itself from within wasm or somehow move
/// more of this information into some other section. Really what this is is
/// sort of an "export map" saying how to wire up all the free functions from
/// the wasm module into the output expected JS module. All our functions here
/// currently take integer parameters and require a JS wrapper, but ideally
/// we'd change them one day to taking/receiving `anyref` which then use some
/// sort of webidl import to customize behavior or something like that. In any
/// case this doesn't feel quite right in terms of priviledge separation, so
/// we'll want to work on this. For now though it works.
#[derive(Debug)]
pub enum AuxExportKind {
    /// A free function that's just listed on the exported module
    Function(String),

    /// A function that's used to create an instane of a class. The function
    /// actually return just an integer which is put on an JS object currently.
    Constructor(String),

    /// This function is intended to be a getter for a field on a class. The
    /// first argument is the internal pointer and the returned value is
    /// expected to be the field.
    Getter { class: String, field: String },

    /// This function is intended to be a setter for a field on a class. The
    /// first argument is the internal pointer and the second argument is
    /// expected to be the field's new value.
    Setter { class: String, field: String },

    /// This is a free function (ish) but scoped inside of a class name.
    StaticFunction { class: String, name: String },

    /// This is a member function of a class where the first parameter is the
    /// implicit integer stored in the class instance.
    Method {
        class: String,
        name: String,
        /// Whether or not this is calling a by-value method in Rust and should
        /// clear the internal pointer in JS automatically.
        consumed: bool,
    },
}

#[derive(Debug)]
pub struct AuxEnum {
    /// The name of this enum
    pub name: String,
    /// The copied Rust comments to forward to JS
    pub comments: String,
    /// A list of variants with their name and value
    pub variants: Vec<(String, u32)>,
}

#[derive(Debug)]
pub struct AuxStruct {
    /// The name of this struct
    pub name: String,
    /// The copied Rust comments to forward to JS
    pub comments: String,
}

/// All possible types of imports that can be imported by a wasm module.
///
/// This `enum` is intended to map out what an imported value is. For example
/// this contains a ton of shims and various ways you can call a function. The
/// base variant here is `Value` which simply means "hook this up to the import"
/// and the signatures will match up.
///
/// Note that this is *not* the same as the webidl bindings section. This is
/// intended to be coupled with that to map out what actually gets hooked up to
/// an import in the wasm module. The two work in tandem.
///
/// Some of these items here are native to JS (like `Value`, indexing
/// operations, etc). Others are shims generated by wasm-bindgen (like `Closure`
/// or `Instanceof`).
#[derive(Debug)]
pub enum AuxImport {
    /// This import is expected to simply be whatever is the value that's
    /// imported
    Value(AuxValue),

    /// This import is expected to be a function that takes an `anyref` and
    /// returns a `bool`. It's expected that it tests if the argument is an
    /// instance of (using `instanceof`) the name specified.
    ///
    /// TODO: can we use `Reflect` or something like that to avoid an extra kind
    /// of import here?
    Instanceof(JsImport),

    /// This import is expected to be a shim that returns the JS value named by
    /// `JsImport`.
    Static(JsImport),

    /// This import is intended to manufacture a JS closure with the given
    /// signature and then return that back to Rust.
    Closure(Closure),

    /// This import is expected to be a shim that simply calls the `foo` method
    /// on the first object, passing along all other parameters and returning
    /// the resulting value.
    StructuralMethod(String),

    /// This import is a "structural getter" which simply returns the `.field`
    /// value of the first argument as an object.
    ///
    /// e.g. `function(x) { return x.foo; }`
    StructuralGetter(String),

    /// This import is a "structural getter" which simply returns the `.field`
    /// value of the specified class
    ///
    /// e.g. `function() { return TheClass.foo; }`
    StructuralClassGetter(JsImport, String),

    /// This import is a "structural setter" which simply sets the `.field`
    /// value of the first argument to the second argument.
    ///
    /// e.g. `function(x, y) { x.foo = y; }`
    StructuralSetter(String),

    /// This import is a "structural setter" which simply sets the `.field`
    /// value of the specified class to the first argument of the function.
    ///
    /// e.g. `function(x) { TheClass.foo = x; }`
    StructuralClassSetter(JsImport, String),

    /// This import is expected to be a shim that is an indexing getter of the
    /// JS class here, where the first argument of the function is the field to
    /// look up. The return value is the value of the field.
    ///
    /// e.g. `function(x) { return TheClass[x]; }`
    ///
    /// TODO: can we use `Reflect` or something like that to avoid an extra kind
    /// of import here?
    IndexingGetterOfClass(JsImport),

    /// This import is expected to be a shim that is an indexing getter of the
    /// first argument interpreted as an object where the field to look up is
    /// the second argument.
    ///
    /// e.g. `function(x, y) { return x[y]; }`
    ///
    /// TODO: can we use `Reflect` or something like that to avoid an extra kind
    /// of import here?
    IndexingGetterOfObject,

    /// This import is expected to be a shim that is an indexing setter of the
    /// JS class here, where the first argument of the function is the field to
    /// set and the second is the value to set it to.
    ///
    /// e.g. `function(x, y) { TheClass[x] = y; }`
    ///
    /// TODO: can we use `Reflect` or something like that to avoid an extra kind
    /// of import here?
    IndexingSetterOfClass(JsImport),

    /// This import is expected to be a shim that is an indexing setter of the
    /// first argument interpreted as an object where the next two fields are
    /// the field to set and the value to set it to.
    ///
    /// e.g. `function(x, y, z) { x[y] = z; }`
    ///
    /// TODO: can we use `Reflect` or something like that to avoid an extra kind
    /// of import here?
    IndexingSetterOfObject,

    /// This import is expected to be a shim that is an indexing deleter of the
    /// JS class here, where the first argument of the function is the field to
    /// delete.
    ///
    /// e.g. `function(x) { delete TheClass[x]; }`
    ///
    /// TODO: can we use `Reflect` or something like that to avoid an extra kind
    /// of import here?
    IndexingDeleterOfClass(JsImport),

    /// This import is expected to be a shim that is an indexing deleter of the
    /// first argument interpreted as an object where the second argument is
    /// the field to delete.
    ///
    /// e.g. `function(x, y) { delete x[y]; }`
    ///
    /// TODO: can we use `Reflect` or something like that to avoid an extra kind
    /// of import here?
    IndexingDeleterOfObject,

    /// This import is a generated shim which will wrap the provided pointer in
    /// a JS object corresponding to the Class name given here. The class name
    /// is one that is exported from the Rust/wasm.
    ///
    /// TODO: sort of like the export map below we should ideally create the
    /// `anyref` from within Rust itself and then return it directly rather than
    /// requiring an intrinsic here to do so.
    WrapInExportedClass(String),

    /// This is an intrinsic function expected to be implemented with a JS glue
    /// shim. Each intrinsic has its own expected signature and implementation.
    Intrinsic(Intrinsic),
}

/// Values that can be imported verbatim to hook up to an import.
#[derive(Debug)]
pub enum AuxValue {
    /// A bare JS value, no transformations, just put it in the slot.
    Bare(JsImport),

    /// A getter function for the class listed for the field, acquired using
    /// `getOwnPropertyDescriptor`.
    Getter(JsImport, String),

    /// Like `Getter`, but accesses a field of a class instead of an instance
    /// of the class.
    ClassGetter(JsImport, String),

    /// Like `Getter`, except the `set` property.
    Setter(JsImport, String),

    /// Like `Setter`, but for class fields instead of instance fields.
    ClassSetter(JsImport, String),
}

/// What can actually be imported and typically a value in each of the variants
/// above of `AuxImport`
///
/// A `JsImport` is intended to indicate what exactly is being imported for a
/// particular operation.
#[derive(Debug, Hash, Eq, PartialEq, Clone)]
pub struct JsImport {
    /// The base of whatever is being imported, either from a module, the global
    /// namespace, or similar.
    pub name: JsImportName,
    /// Various field accesses (like `.foo.bar.baz`) to hang off the `name`
    /// above.
    pub fields: Vec<String>,
}

/// Return value of `determine_import` which is where we look at an imported
/// function AST and figure out where it's actually being imported from
/// (performing some validation checks and whatnot).
#[derive(Debug, Hash, Eq, PartialEq, Clone)]
pub enum JsImportName {
    /// An item is imported from the global scope. The `name` is what's
    /// imported.
    Global { name: String },
    /// Same as `Global`, except the `name` is imported via an ESM import from
    /// the specified `module` path.
    Module { module: String, name: String },
    /// Same as `Module`, except we're importing from a local module defined in
    /// a local JS snippet.
    LocalModule { module: String, name: String },
    /// Same as `Module`, except we're importing from an `inline_js` attribute
    InlineJs {
        unique_crate_identifier: String,
        snippet_idx_in_crate: usize,
        name: String,
    },
    /// A global import which may have a number of vendor prefixes associated
    /// with it, like `webkitAudioPrefix`. The `name` is the name to test
    /// whether it's prefixed.
    VendorPrefixed { name: String, prefixes: Vec<String> },
}

struct Context<'a> {
    start_found: bool,
    module: &'a mut Module,
    bindings: WebidlCustomSection,
    aux: WasmBindgenAux,
    function_exports: HashMap<String, (ExportId, FunctionId)>,
    function_imports: HashMap<String, (ImportId, FunctionId)>,
    vendor_prefixes: HashMap<String, Vec<String>>,
    unique_crate_identifier: &'a str,
    descriptors: HashMap<String, Descriptor>,
}

pub fn process(module: &mut Module) -> Result<(WebidlCustomSectionId, WasmBindgenAuxId), Error> {
    let mut storage = Vec::new();
    let programs = extract_programs(module, &mut storage)?;

    let mut cx = Context {
        bindings: Default::default(),
        aux: Default::default(),
        function_exports: Default::default(),
        function_imports: Default::default(),
        vendor_prefixes: Default::default(),
        descriptors: Default::default(),
        unique_crate_identifier: "",
        module,
        start_found: false,
    };
    cx.init();

    for program in programs {
        cx.program(program)?;
    }

    cx.verify()?;

    let bindings = cx.module.customs.add(cx.bindings);
    let aux = cx.module.customs.add(cx.aux);
    Ok((bindings, aux))
}

impl<'a> Context<'a> {
    fn init(&mut self) {
        // Make a map from string name to ids of all exports
        for export in self.module.exports.iter() {
            if let walrus::ExportItem::Function(f) = export.item {
                self.function_exports
                    .insert(export.name.clone(), (export.id(), f));
            }
        }

        // Make a map from string name to ids of all imports from our
        // placeholder module name which we'll want to be sure that we've got a
        // location listed of what to import there for each item.
        for import in self.module.imports.iter() {
            if import.module != PLACEHOLDER_MODULE {
                continue;
            }
            if let walrus::ImportKind::Function(f) = import.kind {
                self.function_imports
                    .insert(import.name.clone(), (import.id(), f));
                if let Some(intrinsic) = Intrinsic::from_symbol(&import.name) {
                    self.bindings
                        .imports
                        .insert(import.id(), ImportBinding::Function(intrinsic.binding()));
                    self.aux
                        .import_map
                        .insert(import.id(), AuxImport::Intrinsic(intrinsic));
                }
            }
        }

        if let Some(custom) = self
            .module
            .customs
            .delete_typed::<WasmBindgenDescriptorsSection>()
        {
            let WasmBindgenDescriptorsSection {
                descriptors,
                closure_imports,
            } = *custom;
            // Store all the executed descriptors in our own field so we have
            // access to them while processing programs.
            self.descriptors.extend(descriptors);

            // Register all the injected closure imports as that they're expected
            // to manufacture a particular type of closure.
            for (id, descriptor) in closure_imports {
                self.aux
                    .import_map
                    .insert(id, AuxImport::Closure(descriptor));
                let binding = Function {
                    shim_idx: 0,
                    arguments: vec![Descriptor::I32; 3],
                    ret: Descriptor::Anyref,
                };
                self.bindings
                    .imports
                    .insert(id, ImportBinding::Function(binding));
            }
        }
    }

    fn program(&mut self, program: decode::Program<'a>) -> Result<(), Error> {
        self.unique_crate_identifier = program.unique_crate_identifier;
        let decode::Program {
            exports,
            enums,
            imports,
            structs,
            typescript_custom_sections,
            local_modules,
            inline_js,
            unique_crate_identifier,
            package_json,
        } = program;

        for module in local_modules {
            // All local modules we find should be unique, but the same module
            // may have showed up in a few different blocks. If that's the case
            // all the same identifiers should have the same contents.
            if let Some(prev) = self
                .aux
                .local_modules
                .insert(module.identifier.to_string(), module.contents.to_string())
            {
                assert_eq!(prev, module.contents);
            }
        }
        if let Some(s) = package_json {
            self.aux.package_jsons.insert(s.into());
        }
        for export in exports {
            self.export(export)?;
        }

        // Register vendor prefixes for all types before we walk over all the
        // imports to ensure that if a vendor prefix is listed somewhere it'll
        // apply to all the imports.
        for import in imports.iter() {
            if let decode::ImportKind::Type(ty) = &import.kind {
                if ty.vendor_prefixes.len() == 0 {
                    continue;
                }
                self.vendor_prefixes
                    .entry(ty.name.to_string())
                    .or_insert(Vec::new())
                    .extend(ty.vendor_prefixes.iter().map(|s| s.to_string()));
            }
        }
        for import in imports {
            self.import(import)?;
        }

        for enum_ in enums {
            self.enum_(enum_)?;
        }
        for struct_ in structs {
            self.struct_(struct_)?;
        }
        for section in typescript_custom_sections {
            self.aux.extra_typescript.push_str(section);
            self.aux.extra_typescript.push_str("\n\n");
        }
        self.aux
            .snippets
            .entry(unique_crate_identifier.to_string())
            .or_insert(Vec::new())
            .extend(inline_js.iter().map(|s| s.to_string()));
        Ok(())
    }

    fn export(&mut self, export: decode::Export<'_>) -> Result<(), Error> {
        let wasm_name = match &export.class {
            Some(class) => struct_function_export_name(class, export.function.name),
            None => export.function.name.to_string(),
        };
        let mut descriptor = match self.descriptors.remove(&wasm_name) {
            None => return Ok(()),
            Some(d) => d.unwrap_function(),
        };
        let (export_id, id) = self.function_exports[&wasm_name];
        if export.start {
            self.add_start_function(id)?;
        }

        let kind = match export.class {
            Some(class) => {
                let class = class.to_string();
                match export.method_kind {
                    decode::MethodKind::Constructor => AuxExportKind::Constructor(class),
                    decode::MethodKind::Operation(op) => match op.kind {
                        decode::OperationKind::Getter(f) => {
                            descriptor.arguments.insert(0, Descriptor::I32);
                            AuxExportKind::Getter {
                                class,
                                field: f.to_string(),
                            }
                        }
                        decode::OperationKind::Setter(f) => {
                            descriptor.arguments.insert(0, Descriptor::I32);
                            AuxExportKind::Setter {
                                class,
                                field: f.to_string(),
                            }
                        }
                        _ if op.is_static => AuxExportKind::StaticFunction {
                            class,
                            name: export.function.name.to_string(),
                        },
                        _ => {
                            descriptor.arguments.insert(0, Descriptor::I32);
                            AuxExportKind::Method {
                                class,
                                name: export.function.name.to_string(),
                                consumed: export.consumed,
                            }
                        }
                    },
                }
            }
            None => AuxExportKind::Function(export.function.name.to_string()),
        };

        self.aux.export_map.insert(
            export_id,
            AuxExport {
                debug_name: wasm_name,
                comments: concatenate_comments(&export.comments),
                arg_names: Some(export.function.arg_names),
                kind,
            },
        );
        self.bindings.exports.insert(export_id, descriptor);
        Ok(())
    }

    fn add_start_function(&mut self, id: FunctionId) -> Result<(), Error> {
        if self.start_found {
            bail!("cannot specify two `start` functions");
        }
        self.start_found = true;

        let prev_start = match self.module.start {
            Some(f) => f,
            None => {
                self.module.start = Some(id);
                return Ok(());
            }
        };

        // Note that we call the previous start function, if any, first. This is
        // because the start function currently only shows up when it's injected
        // through thread/anyref transforms. These injected start functions need
        // to happen before user code, so we always schedule them first.
        let mut builder = walrus::FunctionBuilder::new();
        let call1 = builder.call(prev_start, Box::new([]));
        let call2 = builder.call(id, Box::new([]));
        let ty = self.module.funcs.get(id).ty();
        let new_start = builder.finish(ty, Vec::new(), vec![call1, call2], self.module);
        self.module.start = Some(new_start);
        Ok(())
    }

    fn import(&mut self, import: decode::Import<'_>) -> Result<(), Error> {
        match &import.kind {
            decode::ImportKind::Function(f) => self.import_function(&import, f),
            decode::ImportKind::Static(s) => self.import_static(&import, s),
            decode::ImportKind::Type(t) => self.import_type(&import, t),
            decode::ImportKind::Enum(_) => Ok(()),
        }
    }

    fn import_function(
        &mut self,
        import: &decode::Import<'_>,
        function: &decode::ImportFunction<'_>,
    ) -> Result<(), Error> {
        let decode::ImportFunction {
            shim,
            catch,
            variadic,
            method,
            structural,
            function,
        } = function;
        let (import_id, _id) = match self.function_imports.get(*shim) {
            Some(pair) => *pair,
            None => return Ok(()),
        };
        let descriptor = match self.descriptors.remove(*shim) {
            None => return Ok(()),
            Some(d) => d.unwrap_function(),
        };

        // Record this for later as it affects JS binding generation, but note
        // that this doesn't affect the WebIDL interface at all.
        if *variadic {
            self.aux.imports_with_variadic.insert(import_id);
        }
        if *catch {
            self.aux.imports_with_catch.insert(import_id);
        }

        // Perform two functions here. First we're saving off our WebIDL
        // bindings signature, indicating what we think our import is going to
        // be. Next we're saving off other metadata indicating where this item
        // is going to be imported from. The `import_map` table will record, for
        // each import, what is getting hooked up to that slot of the import
        // table to the WebAssembly instance.
        let import = match method {
            Some(data) => {
                let class = self.determine_import(import, &data.class)?;
                match &data.kind {
                    // NB: `structural` is ignored for constructors since the
                    // js type isn't expected to change anyway.
                    decode::MethodKind::Constructor => {
                        self.bindings
                            .imports
                            .insert(import_id, ImportBinding::Constructor(descriptor));
                        AuxImport::Value(AuxValue::Bare(class))
                    }
                    decode::MethodKind::Operation(op) => {
                        let (import, method) =
                            self.determine_import_op(class, function, *structural, op)?;
                        let binding = if method {
                            ImportBinding::Method(descriptor)
                        } else {
                            ImportBinding::Function(descriptor)
                        };
                        self.bindings.imports.insert(import_id, binding);
                        import
                    }
                }
            }

            // NB: `structural` is ignored for free functions since it's
            // expected that the binding isn't changing anyway.
            None => {
                self.bindings
                    .imports
                    .insert(import_id, ImportBinding::Function(descriptor));
                let name = self.determine_import(import, function.name)?;
                AuxImport::Value(AuxValue::Bare(name))
            }
        };

        self.aux.import_map.insert(import_id, import);
        Ok(())
    }

    /// The `bool` returned indicates whether the imported value should be
    /// invoked as a method (first arg is implicitly `this`) or if the imported
    /// value is a simple function-like shim
    fn determine_import_op(
        &mut self,
        mut class: JsImport,
        function: &decode::Function<'_>,
        structural: bool,
        op: &decode::Operation<'_>,
    ) -> Result<(AuxImport, bool), Error> {
        match op.kind {
            decode::OperationKind::Regular => {
                if op.is_static {
                    class.fields.push(function.name.to_string());
                    Ok((AuxImport::Value(AuxValue::Bare(class)), false))
                } else if structural {
                    Ok((
                        AuxImport::StructuralMethod(function.name.to_string()),
                        false,
                    ))
                } else {
                    class.fields.push("prototype".to_string());
                    class.fields.push(function.name.to_string());
                    Ok((AuxImport::Value(AuxValue::Bare(class)), true))
                }
            }

            decode::OperationKind::Getter(field) => {
                if structural {
                    if op.is_static {
                        Ok((
                            AuxImport::StructuralClassGetter(class, field.to_string()),
                            false,
                        ))
                    } else {
                        Ok((AuxImport::StructuralGetter(field.to_string()), false))
                    }
                } else {
                    let val = if op.is_static {
                        AuxValue::ClassGetter(class, field.to_string())
                    } else {
                        AuxValue::Getter(class, field.to_string())
                    };
                    Ok((AuxImport::Value(val), true))
                }
            }

            decode::OperationKind::Setter(field) => {
                if structural {
                    if op.is_static {
                        Ok((
                            AuxImport::StructuralClassSetter(class, field.to_string()),
                            false,
                        ))
                    } else {
                        Ok((AuxImport::StructuralSetter(field.to_string()), false))
                    }
                } else {
                    let val = if op.is_static {
                        AuxValue::ClassSetter(class, field.to_string())
                    } else {
                        AuxValue::Setter(class, field.to_string())
                    };
                    Ok((AuxImport::Value(val), true))
                }
            }

            decode::OperationKind::IndexingGetter => {
                if !structural {
                    bail!("indexing getters must always be structural");
                }
                if op.is_static {
                    Ok((AuxImport::IndexingGetterOfClass(class), false))
                } else {
                    Ok((AuxImport::IndexingGetterOfObject, false))
                }
            }

            decode::OperationKind::IndexingSetter => {
                if !structural {
                    bail!("indexing setters must always be structural");
                }
                if op.is_static {
                    Ok((AuxImport::IndexingSetterOfClass(class), false))
                } else {
                    Ok((AuxImport::IndexingSetterOfObject, false))
                }
            }

            decode::OperationKind::IndexingDeleter => {
                if !structural {
                    bail!("indexing deleters must always be structural");
                }
                if op.is_static {
                    Ok((AuxImport::IndexingDeleterOfClass(class), false))
                } else {
                    Ok((AuxImport::IndexingDeleterOfObject, false))
                }
            }
        }
    }

    fn import_static(
        &mut self,
        import: &decode::Import<'_>,
        static_: &decode::ImportStatic<'_>,
    ) -> Result<(), Error> {
        let (import_id, _id) = match self.function_imports.get(static_.shim) {
            Some(pair) => *pair,
            None => return Ok(()),
        };

        // Register the signature of this imported shim
        self.bindings.imports.insert(
            import_id,
            ImportBinding::Function(Function {
                arguments: Vec::new(),
                shim_idx: 0,
                ret: Descriptor::Anyref,
            }),
        );

        // And then save off that this function is is an instanceof shim for an
        // imported item.
        let import = self.determine_import(import, &static_.name)?;
        self.aux
            .import_map
            .insert(import_id, AuxImport::Static(import));
        Ok(())
    }

    fn import_type(
        &mut self,
        import: &decode::Import<'_>,
        type_: &decode::ImportType<'_>,
    ) -> Result<(), Error> {
        let (import_id, _id) = match self.function_imports.get(type_.instanceof_shim) {
            Some(pair) => *pair,
            None => return Ok(()),
        };

        // Register the signature of this imported shim
        self.bindings.imports.insert(
            import_id,
            ImportBinding::Function(Function {
                arguments: vec![Descriptor::Ref(Box::new(Descriptor::Anyref))],
                shim_idx: 0,
                ret: Descriptor::I32,
            }),
        );

        // And then save off that this function is is an instanceof shim for an
        // imported item.
        let import = self.determine_import(import, &type_.name)?;
        self.aux
            .import_map
            .insert(import_id, AuxImport::Instanceof(import));
        Ok(())
    }

    fn enum_(&mut self, enum_: decode::Enum<'_>) -> Result<(), Error> {
        let aux = AuxEnum {
            name: enum_.name.to_string(),
            comments: concatenate_comments(&enum_.comments),
            variants: enum_
                .variants
                .iter()
                .map(|v| (v.name.to_string(), v.value))
                .collect(),
        };
        self.aux.enums.push(aux);
        Ok(())
    }

    fn struct_(&mut self, struct_: decode::Struct<'_>) -> Result<(), Error> {
        for field in struct_.fields {
            let getter = wasm_bindgen_shared::struct_field_get(&struct_.name, &field.name);
            let setter = wasm_bindgen_shared::struct_field_set(&struct_.name, &field.name);
            let descriptor = match self.descriptors.remove(&getter) {
                None => continue,
                Some(d) => d,
            };

            // Register a webidl transformation for the getter
            let (getter_id, _) = self.function_exports[&getter];
            let getter_descriptor = Function {
                arguments: vec![Descriptor::I32],
                shim_idx: 0,
                ret: descriptor.clone(),
            };
            self.bindings.exports.insert(getter_id, getter_descriptor);
            self.aux.export_map.insert(
                getter_id,
                AuxExport {
                    debug_name: format!("getter for `{}::{}`", struct_.name, field.name),
                    arg_names: None,
                    comments: concatenate_comments(&field.comments),
                    kind: AuxExportKind::Getter {
                        class: struct_.name.to_string(),
                        field: field.name.to_string(),
                    },
                },
            );

            // If present, register information for the setter as well.
            if field.readonly {
                continue;
            }

            let (setter_id, _) = self.function_exports[&setter];
            let setter_descriptor = Function {
                arguments: vec![Descriptor::I32, descriptor],
                shim_idx: 0,
                ret: Descriptor::Unit,
            };
            self.bindings.exports.insert(setter_id, setter_descriptor);
            self.aux.export_map.insert(
                setter_id,
                AuxExport {
                    debug_name: format!("setter for `{}::{}`", struct_.name, field.name),
                    arg_names: None,
                    comments: concatenate_comments(&field.comments),
                    kind: AuxExportKind::Setter {
                        class: struct_.name.to_string(),
                        field: field.name.to_string(),
                    },
                },
            );
        }
        let aux = AuxStruct {
            name: struct_.name.to_string(),
            comments: concatenate_comments(&struct_.comments),
        };
        self.aux.structs.push(aux);

        let wrap_constructor = wasm_bindgen_shared::new_function(struct_.name);
        if let Some((import_id, _id)) = self.function_imports.get(&wrap_constructor) {
            self.aux.import_map.insert(
                *import_id,
                AuxImport::WrapInExportedClass(struct_.name.to_string()),
            );
            let binding = Function {
                shim_idx: 0,
                arguments: vec![Descriptor::I32],
                ret: Descriptor::Anyref,
            };
            self.bindings
                .imports
                .insert(*import_id, ImportBinding::Function(binding));
        }

        Ok(())
    }

    fn determine_import(&self, import: &decode::Import<'_>, item: &str) -> Result<JsImport, Error> {
        let is_local_snippet = match import.module {
            decode::ImportModule::Named(s) => self.aux.local_modules.contains_key(s),
            decode::ImportModule::RawNamed(_) => false,
            decode::ImportModule::Inline(_) => true,
            decode::ImportModule::None => false,
        };

        // Similar to `--target no-modules`, only allow vendor prefixes
        // basically for web apis, shouldn't be necessary for things like npm
        // packages or other imported items.
        let vendor_prefixes = self.vendor_prefixes.get(item);
        if let Some(vendor_prefixes) = vendor_prefixes {
            assert!(vendor_prefixes.len() > 0);

            if is_local_snippet {
                bail!(
                    "local JS snippets do not support vendor prefixes for \
                     the import of `{}` with a polyfill of `{}`",
                    item,
                    &vendor_prefixes[0]
                );
            }
            if let decode::ImportModule::Named(module) = &import.module {
                bail!(
                    "import of `{}` from `{}` has a polyfill of `{}` listed, but
                     vendor prefixes aren't supported when importing from modules",
                    item,
                    module,
                    &vendor_prefixes[0],
                );
            }
            if let Some(ns) = &import.js_namespace {
                bail!(
                    "import of `{}` through js namespace `{}` isn't supported \
                     right now when it lists a polyfill",
                    item,
                    ns
                );
            }
            return Ok(JsImport {
                name: JsImportName::VendorPrefixed {
                    name: item.to_string(),
                    prefixes: vendor_prefixes.clone(),
                },
                fields: Vec::new(),
            });
        }

        let (name, fields) = match import.js_namespace {
            Some(ns) => (ns, vec![item.to_string()]),
            None => (item, Vec::new()),
        };

        let name = match import.module {
            decode::ImportModule::Named(module) if is_local_snippet => JsImportName::LocalModule {
                module: module.to_string(),
                name: name.to_string(),
            },
            decode::ImportModule::Named(module) | decode::ImportModule::RawNamed(module) => {
                JsImportName::Module {
                    module: module.to_string(),
                    name: name.to_string(),
                }
            }
            decode::ImportModule::Inline(idx) => {
                let offset = self
                    .aux
                    .snippets
                    .get(self.unique_crate_identifier)
                    .map(|s| s.len())
                    .unwrap_or(0);
                JsImportName::InlineJs {
                    unique_crate_identifier: self.unique_crate_identifier.to_string(),
                    snippet_idx_in_crate: idx as usize + offset,
                    name: name.to_string(),
                }
            }
            decode::ImportModule::None => JsImportName::Global {
                name: name.to_string(),
            },
        };
        Ok(JsImport { name, fields })
    }

    /// Perform a small verification pass over the module to perform some
    /// internal sanity checks.
    fn verify(&self) -> Result<(), Error> {
        let mut imports_counted = 0;
        for import in self.module.imports.iter() {
            if import.module != PLACEHOLDER_MODULE {
                continue;
            }
            match import.kind {
                walrus::ImportKind::Function(_) => {}
                _ => bail!("import from `{}` was not a function", PLACEHOLDER_MODULE),
            }

            // Ensure that everything imported from the `__wbindgen_placeholder__`
            // module has a location listed as to where it's expected to be
            // imported from.
            if !self.aux.import_map.contains_key(&import.id()) {
                bail!(
                    "import of `{}` doesn't have an import map item listed",
                    import.name
                );
            }

            // Also make sure there's a binding listed for it.
            if !self.bindings.imports.contains_key(&import.id()) {
                bail!("import of `{}` doesn't have a binding listed", import.name);
            }
            imports_counted += 1;
        }

        // Make sure there's no extraneous bindings that weren't actually
        // imported in the module.
        if self.aux.import_map.len() != imports_counted {
            bail!("import map is larger than the number of imports");
        }
        if self.bindings.imports.len() != imports_counted {
            bail!("import binding map is larger than the number of imports");
        }

        // Make sure the export map and export bindings map contain the same
        // number of entries.
        for id in self.bindings.exports.keys() {
            if !self.aux.export_map.contains_key(id) {
                bail!("bindings map has an entry that the export map does not");
            }
        }

        if self.bindings.exports.len() != self.aux.export_map.len() {
            bail!("export map and export bindings map have different sizes");
        }

        Ok(())
    }
}

impl walrus::CustomSection for WebidlCustomSection {
    fn name(&self) -> &str {
        "webidl custom section"
    }

    fn data(&self, _: &walrus::IdsToIndices) -> Cow<[u8]> {
        panic!("shouldn't emit custom sections just yet");
    }
}

impl walrus::CustomSection for WasmBindgenAux {
    fn name(&self) -> &str {
        "wasm-bindgen custom section"
    }

    fn data(&self, _: &walrus::IdsToIndices) -> Cow<[u8]> {
        panic!("shouldn't emit custom sections just yet");
    }
}

fn extract_programs<'a>(
    module: &mut Module,
    program_storage: &'a mut Vec<Vec<u8>>,
) -> Result<Vec<decode::Program<'a>>, Error> {
    let my_version = wasm_bindgen_shared::version();
    assert!(program_storage.is_empty());

    while let Some(raw) = module.customs.remove_raw("__wasm_bindgen_unstable") {
        log::debug!(
            "custom section '{}' looks like a wasm bindgen section",
            raw.name
        );
        program_storage.push(raw.data);
    }

    let mut ret = Vec::new();
    for program in program_storage.iter() {
        let mut payload = &program[..];
        while let Some(data) = get_remaining(&mut payload) {
            // Historical versions of wasm-bindgen have used JSON as the custom
            // data section format. Newer versions, however, are using a custom
            // serialization protocol that looks much more like the wasm spec.
            //
            // We, however, want a sanity check to ensure that if we're running
            // against the wrong wasm-bindgen we get a nicer error than an
            // internal decode error. To that end we continue to verify a tiny
            // bit of json at the beginning of each blob before moving to the
            // next blob. This should keep us compatible with older wasm-bindgen
            // instances as well as forward-compatible for now.
            //
            // Note, though, that as `wasm-pack` picks up steam it's hoped we
            // can just delete this entirely. The `wasm-pack` project already
            // manages versions for us, so we in theory should need this check
            // less and less over time.
            if let Some(their_version) = verify_schema_matches(data)? {
                bail!(
                    "

it looks like the Rust project used to create this wasm file was linked against
a different version of wasm-bindgen than this binary:

  rust wasm file: {}
     this binary: {}

Currently the bindgen format is unstable enough that these two version must
exactly match, so it's required that these two version are kept in sync by
either updating the wasm-bindgen dependency or this binary. You should be able
to update the wasm-bindgen dependency with:

    cargo update -p wasm-bindgen

or you can update the binary with

    cargo install -f wasm-bindgen-cli

if this warning fails to go away though and you're not sure what to do feel free
to open an issue at https://github.com/rustwasm/wasm-bindgen/issues!
",
                    their_version,
                    my_version,
                );
            }
            let next = get_remaining(&mut payload).unwrap();
            log::debug!("found a program of length {}", next.len());
            ret.push(<decode::Program as decode::Decode>::decode_all(next));
        }
    }
    Ok(ret)
}

fn get_remaining<'a>(data: &mut &'a [u8]) -> Option<&'a [u8]> {
    if data.len() == 0 {
        return None;
    }
    let len = ((data[0] as usize) << 0)
        | ((data[1] as usize) << 8)
        | ((data[2] as usize) << 16)
        | ((data[3] as usize) << 24);
    let (a, b) = data[4..].split_at(len);
    *data = b;
    Some(a)
}

fn verify_schema_matches<'a>(data: &'a [u8]) -> Result<Option<&'a str>, Error> {
    macro_rules! bad {
        () => {
            bail!("failed to decode what looked like wasm-bindgen data")
        };
    }
    let data = match str::from_utf8(data) {
        Ok(s) => s,
        Err(_) => bad!(),
    };
    log::debug!("found version specifier {}", data);
    if !data.starts_with("{") || !data.ends_with("}") {
        bad!()
    }
    let needle = "\"schema_version\":\"";
    let rest = match data.find(needle) {
        Some(i) => &data[i + needle.len()..],
        None => bad!(),
    };
    let their_schema_version = match rest.find("\"") {
        Some(i) => &rest[..i],
        None => bad!(),
    };
    if their_schema_version == wasm_bindgen_shared::SCHEMA_VERSION {
        return Ok(None);
    }
    let needle = "\"version\":\"";
    let rest = match data.find(needle) {
        Some(i) => &data[i + needle.len()..],
        None => bad!(),
    };
    let their_version = match rest.find("\"") {
        Some(i) => &rest[..i],
        None => bad!(),
    };
    Ok(Some(their_version))
}

fn concatenate_comments(comments: &[&str]) -> String {
    comments
        .iter()
        .map(|s| s.trim_matches('"'))
        .collect::<Vec<_>>()
        .join("\n")
}
