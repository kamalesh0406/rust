/**
   Code that is useful in various trans modules.

*/

import std::int;
import std::str;
import std::uint;
import std::str::rustrt::sbuf;
import std::map;
import std::map::hashmap;
import std::option;
import std::option::some;
import std::option::none;
import std::fs;
import syntax::ast;
import syntax::walk;
import driver::session;
import middle::ty;
import back::link;
import back::x86;
import back::abi;
import back::upcall;
import syntax::visit;
import visit::vt;
import util::common;
import util::common::*;
import std::map::new_int_hash;
import std::map::new_str_hash;
import syntax::codemap::span;
import lib::llvm::llvm;
import lib::llvm::builder;
import lib::llvm::target_data;
import lib::llvm::type_names;
import lib::llvm::mk_target_data;
import lib::llvm::mk_type_names;
import lib::llvm::llvm::ModuleRef;
import lib::llvm::llvm::ValueRef;
import lib::llvm::llvm::TypeRef;
import lib::llvm::llvm::TypeHandleRef;
import lib::llvm::llvm::BuilderRef;
import lib::llvm::llvm::BasicBlockRef;
import lib::llvm::False;
import lib::llvm::True;
import lib::llvm::Bool;
import link::mangle_internal_name_by_type_only;
import link::mangle_internal_name_by_seq;
import link::mangle_internal_name_by_path;
import link::mangle_internal_name_by_path_and_seq;
import link::mangle_exported_name;
import metadata::creader;
import metadata::csearch;
import metadata::cstore;
import util::ppaux::ty_to_str;
import util::ppaux::ty_to_short_str;
import syntax::print::pprust::expr_to_str;
import syntax::print::pprust::path_to_str;

// FIXME: These should probably be pulled in here too.
import trans::type_of_fn_full;
import trans::drop_slot;
import trans::drop_ty;

obj namegen(mutable i: int) {
    fn next(prefix: str) -> str { i += 1; ret prefix + int::str(i); }
}

type derived_tydesc_info = {lltydesc: ValueRef, escapes: bool};

type glue_fns = {no_op_type_glue: ValueRef};

type tydesc_info =
    {ty: ty::t,
     tydesc: ValueRef,
     size: ValueRef,
     align: ValueRef,
     mutable copy_glue: option::t[ValueRef],
     mutable drop_glue: option::t[ValueRef],
     mutable free_glue: option::t[ValueRef],
     mutable cmp_glue: option::t[ValueRef],
     ty_params: uint[]};

/*
 * A note on nomenclature of linking: "upcall", "extern" and "native".
 *
 * An "extern" is an LLVM symbol we wind up emitting an undefined external
 * reference to. This means "we don't have the thing in this compilation unit,
 * please make sure you link it in at runtime". This could be a reference to
 * C code found in a C library, or rust code found in a rust crate.
 *
 * A "native" is an extern that references C code. Called with cdecl.
 *
 * An upcall is a native call generated by the compiler (not corresponding to
 * any user-written call in the code) into librustrt, to perform some helper
 * task such as bringing a task to life, allocating memory, etc.
 *
 */
type stats =
    {mutable n_static_tydescs: uint,
     mutable n_derived_tydescs: uint,
     mutable n_glues_created: uint,
     mutable n_null_glues: uint,
     mutable n_real_glues: uint,
     fn_times: @mutable {ident: str, time: int}[]};

// Crate context.  Every crate we compile has one of these.
type crate_ctxt =

    // A mapping from the def_id of each item in this crate to the address
    // of the first instruction of the item's definition in the executable
    // we're generating.

    // TODO: hashmap[tup(tag_id,subtys), @tag_info]
    {sess: session::session,
     llmod: ModuleRef,
     td: target_data,
     tn: type_names,
     externs: hashmap[str, ValueRef],
     intrinsics: hashmap[str, ValueRef],
     item_ids: hashmap[ast::node_id, ValueRef],
     ast_map: ast_map::map,
     item_symbols: hashmap[ast::node_id, str],
     mutable main_fn: option::t[ValueRef],
     link_meta: link::link_meta,
     tag_sizes: hashmap[ty::t, uint],
     discrims: hashmap[ast::node_id, ValueRef],
     discrim_symbols: hashmap[ast::node_id, str],
     fn_pairs: hashmap[ast::node_id, ValueRef],
     consts: hashmap[ast::node_id, ValueRef],
     obj_methods: hashmap[ast::node_id, ()],
     tydescs: hashmap[ty::t, @tydesc_info],
     module_data: hashmap[str, ValueRef],
     lltypes: hashmap[ty::t, TypeRef],
     glues: @glue_fns,
     names: namegen,
     sha: std::sha1::sha1,
     type_sha1s: hashmap[ty::t, str],
     type_short_names: hashmap[ty::t, str],
     tcx: ty::ctxt,
     stats: stats,
     upcalls: @upcall::upcalls,
     rust_object_type: TypeRef,
     tydesc_type: TypeRef,
     task_type: TypeRef};

type local_ctxt =
    {path: str[],
     module_path: str[],
     obj_typarams: ast::ty_param[],
     obj_fields: ast::obj_field[],
     ccx: @crate_ctxt};

// Types used for llself.
type val_self_pair = {v: ValueRef, t: ty::t};

// Function context.  Every LLVM function we create will have one of these.
type fn_ctxt =
    // The ValueRef returned from a call to llvm::LLVMAddFunction; the
    // address of the first instruction in the sequence of instructions
    // for this function that will go in the .text section of the
    // executable we're generating.

    // The three implicit arguments that arrive in the function we're
    // creating.  For instance, foo(int, int) is really foo(ret*, task*,
    // env*, int, int).  These are also available via
    // llvm::LLVMGetParam(llfn, uint) where uint = 1, 2, 0 respectively,
    // but we unpack them into these fields for convenience.

    // Points to the current task.

    // Points to the current environment (bindings of variables to
    // values), if this is a regular function; points to the current
    // object, if this is a method.

    // Points to where the return value of this function should end up.

    // The next three elements: "hoisted basic blocks" containing
    // administrative activities that have to happen in only one place in
    // the function, due to LLVM's quirks.

    // A block for all the function's static allocas, so that LLVM will
    // coalesce them into a single alloca call.

    // A block containing code that copies incoming arguments to space
    // already allocated by code in one of the llallocas blocks.  (LLVM
    // requires that arguments be copied to local allocas before allowing
    // most any operation to be performed on them.)

    // The first block containing derived tydescs received from the
    // runtime.  See description of derived_tydescs, below.

    // The last block of the llderivedtydescs group.

    // A block for all of the dynamically sized allocas.  This must be
    // after llderivedtydescs, because these sometimes depend on
    // information computed from derived tydescs.

    // FIXME: Is llcopyargs actually the block containing the allocas for
    // incoming function arguments?  Or is it merely the block containing
    // code that copies incoming args to space already alloca'd by code in
    // llallocas?

    // The 'self' object currently in use in this function, if there is
    // one.

    // If this function is actually a iter, a block containing the code
    // called whenever the iter calls 'put'.

    // The next four items: hash tables mapping from AST def_ids to
    // LLVM-stuff-in-the-frame.

    // Maps arguments to allocas created for them in llallocas.

    // Maps fields in objects to pointers into the interior of llself's
    // body.

    // Maps the def_ids for local variables to the allocas created for
    // them in llallocas.

    // The same as above, but for variables accessed via the frame pointer
    // we pass into an iter, for access to the static environment of the
    // iter-calling frame.

    // For convenience, a vector of the incoming tydescs for each of this
    // functions type parameters, fetched via llvm::LLVMGetParam.  For
    // example, for a function foo[A, B, C](), lltydescs contains the
    // ValueRefs for the tydescs for A, B, and C.

    // Derived tydescs are tydescs created at runtime, for types that
    // involve type parameters inside type constructors.  For example,
    // suppose a function parameterized by T creates a vector of type
    // [T].  The function doesn't know what T is until runtime, and the
    // function's caller knows T but doesn't know that a vector is
    // involved.  So a tydesc for [T] can't be created until runtime,
    // when information about both "[T]" and "T" are available.  When such
    // a tydesc is created, we cache it in the derived_tydescs table for
    // the next time that such a tydesc is needed.

    // The source span where this function comes from, for error
    // reporting.

    // This function's enclosing local context.
    {llfn: ValueRef,
     lltaskptr: ValueRef,
     llenv: ValueRef,
     llretptr: ValueRef,
     mutable llstaticallocas: BasicBlockRef,
     mutable llcopyargs: BasicBlockRef,
     mutable llderivedtydescs_first: BasicBlockRef,
     mutable llderivedtydescs: BasicBlockRef,
     mutable lldynamicallocas: BasicBlockRef,
     mutable llself: option::t[val_self_pair],
     mutable lliterbody: option::t[ValueRef],
     llargs: hashmap[ast::node_id, ValueRef],
     llobjfields: hashmap[ast::node_id, ValueRef],
     lllocals: hashmap[ast::node_id, ValueRef],
     llupvars: hashmap[ast::node_id, ValueRef],
     mutable lltydescs: ValueRef[],
     derived_tydescs: hashmap[ty::t, derived_tydesc_info],
     sp: span,
     lcx: @local_ctxt};

tag cleanup {
    clean(fn(&@block_ctxt) -> result );
    clean_temp(ValueRef, fn(&@block_ctxt) -> result );
}

fn add_clean(cx: &@block_ctxt, val: ValueRef, ty: ty::t) {
    find_scope_cx(cx).cleanups += ~[clean(bind drop_slot(_, val, ty))];
}
fn add_clean_temp(cx: &@block_ctxt, val: ValueRef, ty: ty::t) {
    find_scope_cx(cx).cleanups +=
        ~[clean_temp(val, bind drop_ty(_, val, ty))];
}

// Note that this only works for temporaries. We should, at some point, move
// to a system where we can also cancel the cleanup on local variables, but
// this will be more involved. For now, we simply zero out the local, and the
// drop glue checks whether it is zero.
fn revoke_clean(cx: &@block_ctxt, val: ValueRef) {
    let sc_cx = find_scope_cx(cx);
    let found = -1;
    let i = 0;
    for c: cleanup  in sc_cx.cleanups {
        alt c {
          clean_temp(v, _) {
            if v as uint == val as uint { found = i; break; }
          }
          _ { }
        }
        i += 1;
    }
    // The value does not have a cleanup associated with it. Might be a
    // constant or some immediate value.
    if found == -1 { ret; }
    // We found the cleanup and remove it
    sc_cx.cleanups =
        std::ivec::slice(sc_cx.cleanups, 0u, found as uint) +
            std::ivec::slice(sc_cx.cleanups, (found as uint) + 1u,
                             std::ivec::len(sc_cx.cleanups));
}

tag block_kind {


    // A scope block is a basic block created by translating a block { ... }
    // the the source language.  Since these blocks create variable scope, any
    // variables created in them that are still live at the end of the block
    // must be dropped and cleaned up when the block ends.
    SCOPE_BLOCK;


    // A basic block created from the body of a loop.  Contains pointers to
    // which block to jump to in the case of "continue" or "break", with the
    // "continue" block optional, because "while" and "do while" don't support
    // "continue" (TODO: is this intentional?)
    LOOP_SCOPE_BLOCK(option::t[@block_ctxt], @block_ctxt);


    // A non-scope block is a basic block created as a translation artifact
    // from translating code that expresses conditional logic rather than by
    // explicit { ... } block structure in the source language.  It's called a
    // non-scope block because it doesn't introduce a new variable scope.
    NON_SCOPE_BLOCK;
}


// Basic block context.  We create a block context for each basic block
// (single-entry, single-exit sequence of instructions) we generate from Rust
// code.  Each basic block we generate is attached to a function, typically
// with many basic blocks per function.  All the basic blocks attached to a
// function are organized as a directed graph.
type block_ctxt =
    // The BasicBlockRef returned from a call to
    // llvm::LLVMAppendBasicBlock(llfn, name), which adds a basic block to
    // the function pointed to by llfn.  We insert instructions into that
    // block by way of this block context.

    // The llvm::builder object serving as an interface to LLVM's
    // LLVMBuild* functions.

    // The block pointing to this one in the function's digraph.

    // The 'kind' of basic block this is.

    // A list of functions that run at the end of translating this block,
    // cleaning up any variables that were introduced in the block and
    // need to go out of scope at the end of it.

    // The source span where this block comes from, for error reporting.

    // The function context for the function to which this block is
    // attached.
    {llbb: BasicBlockRef,
     build: builder,
     parent: block_parent,
     kind: block_kind,
     mutable cleanups: cleanup[],
     sp: span,
     fcx: @fn_ctxt};

// FIXME: we should be able to use option::t[@block_parent] here but
// the infinite-tag check in rustboot gets upset.
tag block_parent { parent_none; parent_some(@block_ctxt); }

type result = {bcx: @block_ctxt, val: ValueRef};
type result_t = {bcx: @block_ctxt, val: ValueRef, ty: ty::t};

fn extend_path(cx: @local_ctxt, name: &str) -> @local_ctxt {
    ret @{path: cx.path + ~[name] with *cx};
}

fn rslt(bcx: @block_ctxt, val: ValueRef) -> result {
    ret {bcx: bcx, val: val};
}

fn ty_str(tn: type_names, t: TypeRef) -> str {
    ret lib::llvm::type_to_str(tn, t);
}

fn val_ty(v: ValueRef) -> TypeRef { ret llvm::LLVMTypeOf(v); }

fn val_str(tn: type_names, v: ValueRef) -> str { ret ty_str(tn, val_ty(v)); }

// Returns the nth element of the given LLVM structure type.
fn struct_elt(llstructty: TypeRef, n: uint) -> TypeRef {
    let elt_count = llvm::LLVMCountStructElementTypes(llstructty);
    assert (n < elt_count);
    let elt_tys = std::ivec::init_elt(T_nil(), elt_count);
    llvm::LLVMGetStructElementTypes(llstructty, std::ivec::to_ptr(elt_tys));
    ret llvm::LLVMGetElementType(elt_tys.(n));
}

fn find_scope_cx(cx: &@block_ctxt) -> @block_ctxt {
    if cx.kind != NON_SCOPE_BLOCK { ret cx; }
    alt cx.parent {
      parent_some(b) { ret find_scope_cx(b); }
      parent_none. {
        cx.fcx.lcx.ccx.sess.bug("trans::find_scope_cx() " +
                                    "called on parentless block_ctxt");
      }
    }
}

// Accessors
// TODO: When we have overloading, simplify these names!

fn bcx_tcx(bcx: &@block_ctxt) -> ty::ctxt { ret bcx.fcx.lcx.ccx.tcx; }
fn bcx_ccx(bcx: &@block_ctxt) -> @crate_ctxt { ret bcx.fcx.lcx.ccx; }
fn bcx_lcx(bcx: &@block_ctxt) -> @local_ctxt { ret bcx.fcx.lcx; }
fn bcx_fcx(bcx: &@block_ctxt) -> @fn_ctxt { ret bcx.fcx; }
fn lcx_ccx(lcx: &@local_ctxt) -> @crate_ctxt { ret lcx.ccx; }
fn ccx_tcx(ccx: &@crate_ctxt) -> ty::ctxt { ret ccx.tcx; }

// LLVM type constructors.
fn T_void() -> TypeRef {
    // Note: For the time being llvm is kinda busted here, it has the notion
    // of a 'void' type that can only occur as part of the signature of a
    // function, but no general unit type of 0-sized value. This is, afaict,
    // vestigial from its C heritage, and we'll be attempting to submit a
    // patch upstream to fix it. In the mean time we only model function
    // outputs (Rust functions and C functions) using T_void, and model the
    // Rust general purpose nil type you can construct as 1-bit (always
    // zero). This makes the result incorrect for now -- things like a tuple
    // of 10 nil values will have 10-bit size -- but it doesn't seem like we
    // have any other options until it's fixed upstream.

    ret llvm::LLVMVoidType();
}

fn T_nil() -> TypeRef {
    // NB: See above in T_void().

    ret llvm::LLVMInt1Type();
}

fn T_i1() -> TypeRef { ret llvm::LLVMInt1Type(); }

fn T_i8() -> TypeRef { ret llvm::LLVMInt8Type(); }

fn T_i16() -> TypeRef { ret llvm::LLVMInt16Type(); }

fn T_i32() -> TypeRef { ret llvm::LLVMInt32Type(); }

fn T_i64() -> TypeRef { ret llvm::LLVMInt64Type(); }

fn T_f32() -> TypeRef { ret llvm::LLVMFloatType(); }

fn T_f64() -> TypeRef { ret llvm::LLVMDoubleType(); }

fn T_bool() -> TypeRef { ret T_i1(); }

fn T_int() -> TypeRef {
    // FIXME: switch on target type.

    ret T_i32();
}

fn T_float() -> TypeRef {
    // FIXME: switch on target type.
    ret T_f64();
}

fn T_char() -> TypeRef { ret T_i32(); }

fn T_size_t() -> TypeRef {
    // FIXME: switch on target type.

    ret T_i32();
}

fn T_fn(inputs: &TypeRef[], output: TypeRef) -> TypeRef {
    ret llvm::LLVMFunctionType(output, std::ivec::to_ptr(inputs),
                               std::ivec::len[TypeRef](inputs), False);
}

fn T_fn_pair(cx: &crate_ctxt, tfn: TypeRef) -> TypeRef {
    ret T_struct(~[T_ptr(tfn), T_opaque_closure_ptr(cx)]);
}

fn T_ptr(t: TypeRef) -> TypeRef { ret llvm::LLVMPointerType(t, 0u); }

fn T_struct(elts: &TypeRef[]) -> TypeRef {
    ret llvm::LLVMStructType(std::ivec::to_ptr(elts), std::ivec::len(elts),
                             False);
}

fn T_named_struct(name: &str) -> TypeRef {
    let c = llvm::LLVMGetGlobalContext();
    ret llvm::LLVMStructCreateNamed(c, str::buf(name));
}

fn set_struct_body(t: TypeRef, elts: &TypeRef[]) {
    llvm::LLVMStructSetBody(t, std::ivec::to_ptr(elts), std::ivec::len(elts),
                            False);
}

fn T_empty_struct() -> TypeRef { ret T_struct(~[]); }

fn T_rust_object() -> TypeRef {
    let t = T_named_struct("rust_object");
    let e = T_ptr(T_empty_struct());
    set_struct_body(t, ~[e, e]);
    ret t;
}

fn T_task() -> TypeRef {
    let t = T_named_struct("task");

    let  // Refcount
         // Delegate pointer
         // Stack segment pointer
         // Runtime SP
         // Rust SP
         // GC chain

         // Domain pointer
         // Crate cache pointer
         elems =
        ~[T_int(), T_int(), T_int(), T_int(), T_int(), T_int(), T_int(),
          T_int()];
    set_struct_body(t, elems);
    ret t;
}

fn T_tydesc_field(cx: &crate_ctxt, field: int) -> TypeRef {
    // Bit of a kludge: pick the fn typeref out of the tydesc..

    let tydesc_elts: TypeRef[] =
        std::ivec::init_elt[TypeRef](T_nil(), abi::n_tydesc_fields as uint);
    llvm::LLVMGetStructElementTypes(cx.tydesc_type,
                                    std::ivec::to_ptr[TypeRef](tydesc_elts));
    let t = llvm::LLVMGetElementType(tydesc_elts.(field));
    ret t;
}

fn T_glue_fn(cx: &crate_ctxt) -> TypeRef {
    let s = "glue_fn";
    if cx.tn.name_has_type(s) { ret cx.tn.get_type(s); }
    let t = T_tydesc_field(cx, abi::tydesc_field_drop_glue);
    cx.tn.associate(s, t);
    ret t;
}

fn T_cmp_glue_fn(cx: &crate_ctxt) -> TypeRef {
    let s = "cmp_glue_fn";
    if cx.tn.name_has_type(s) { ret cx.tn.get_type(s); }
    let t = T_tydesc_field(cx, abi::tydesc_field_cmp_glue);
    cx.tn.associate(s, t);
    ret t;
}

fn T_tydesc(taskptr_type: TypeRef) -> TypeRef {
    let tydesc = T_named_struct("tydesc");
    let tydescpp = T_ptr(T_ptr(tydesc));
    let pvoid = T_ptr(T_i8());
    let glue_fn_ty =
        T_ptr(T_fn(~[T_ptr(T_nil()), taskptr_type, T_ptr(T_nil()), tydescpp,
                     pvoid], T_void()));
    let cmp_glue_fn_ty =
        T_ptr(T_fn(~[T_ptr(T_i1()), taskptr_type, T_ptr(T_nil()), tydescpp,
                     pvoid, pvoid, T_i8()], T_void()));

    let  // first_param
         // size
         // align
         // copy_glue
         // drop_glue
         // free_glue
         // sever_glue
         // mark_glue
         // obj_drop_glue
         // is_stateful
        elems =
        ~[tydescpp, T_int(), T_int(), glue_fn_ty, glue_fn_ty, glue_fn_ty,
          glue_fn_ty, glue_fn_ty, glue_fn_ty, glue_fn_ty, cmp_glue_fn_ty];
    set_struct_body(tydesc, elems);
    ret tydesc;
}

fn T_array(t: TypeRef, n: uint) -> TypeRef { ret llvm::LLVMArrayType(t, n); }

fn T_vec(t: TypeRef) -> TypeRef {
    ret T_struct(~[T_int(), // Refcount
                   T_int(), // Alloc
                   T_int(), // Fill

                   T_int(), // Pad
                            // Body elements
                             T_array(t, 0u)]);
}

fn T_opaque_vec_ptr() -> TypeRef { ret T_ptr(T_vec(T_int())); }


// Interior vector.
//
// TODO: Support user-defined vector sizes.
fn T_ivec(t: TypeRef) -> TypeRef {
    ret T_struct(~[T_int(), // Length ("fill"; if zero, heapified)
                   T_int(), // Alloc
                   T_array(t, abi::ivec_default_length)]); // Body elements

}


// Note that the size of this one is in bytes.
fn T_opaque_ivec() -> TypeRef {
    ret T_struct(~[T_int(), // Length ("fill"; if zero, heapified)
                   T_int(), // Alloc
                   T_array(T_i8(), 0u)]); // Body elements

}

fn T_ivec_heap_part(t: TypeRef) -> TypeRef {
    ret T_struct(~[T_int(), // Real length
                   T_array(t, 0u)]); // Body elements

}


// Interior vector on the heap, also known as the "stub". Cast to this when
// the allocated length (second element of T_ivec above) is zero.
fn T_ivec_heap(t: TypeRef) -> TypeRef {
    ret T_struct(~[T_int(), // Length (zero)
                   T_int(), // Alloc
                   T_ptr(T_ivec_heap_part(t))]); // Pointer

}

fn T_opaque_ivec_heap_part() -> TypeRef {
    ret T_struct(~[T_int(), // Real length
                   T_array(T_i8(), 0u)]); // Body elements

}

fn T_opaque_ivec_heap() -> TypeRef {
    ret T_struct(~[T_int(), // Length (zero)
                   T_int(), // Alloc
                   T_ptr(T_opaque_ivec_heap_part())]); // Pointer

}

fn T_str() -> TypeRef { ret T_vec(T_i8()); }

fn T_box(t: TypeRef) -> TypeRef { ret T_struct(~[T_int(), t]); }

fn T_port(t: TypeRef) -> TypeRef {
    ret T_struct(~[T_int()]); // Refcount

}

fn T_chan(t: TypeRef) -> TypeRef {
    ret T_struct(~[T_int()]); // Refcount

}

fn T_taskptr(cx: &crate_ctxt) -> TypeRef { ret T_ptr(cx.task_type); }


// This type must never be used directly; it must always be cast away.
fn T_typaram(tn: &type_names) -> TypeRef {
    let s = "typaram";
    if tn.name_has_type(s) { ret tn.get_type(s); }
    let t = T_i8();
    tn.associate(s, t);
    ret t;
}

fn T_typaram_ptr(tn: &type_names) -> TypeRef { ret T_ptr(T_typaram(tn)); }

fn T_closure_ptr(cx: &crate_ctxt, llbindings_ty: TypeRef,
                 n_ty_params: uint) -> TypeRef {
    // NB: keep this in sync with code in trans_bind; we're making
    // an LLVM typeref structure that has the same "shape" as the ty::t
    // it constructs.
    ret T_ptr(T_box(T_struct(~[T_ptr(cx.tydesc_type),
                               llbindings_ty,
                               T_captured_tydescs(cx, n_ty_params)])));
}

fn T_opaque_closure_ptr(cx: &crate_ctxt) -> TypeRef {
    let s = "*closure";
    if cx.tn.name_has_type(s) { ret cx.tn.get_type(s); }
    let t = T_closure_ptr(cx, T_nil(), 0u);
    cx.tn.associate(s, t);
    ret t;
}

fn T_tag(tn: &type_names, size: uint) -> TypeRef {
    let s = "tag_" + uint::to_str(size, 10u);
    if tn.name_has_type(s) { ret tn.get_type(s); }
    let t = T_struct(~[T_int(), T_array(T_i8(), size)]);
    tn.associate(s, t);
    ret t;
}

fn T_opaque_tag(tn: &type_names) -> TypeRef {
    let s = "opaque_tag";
    if tn.name_has_type(s) { ret tn.get_type(s); }
    let t = T_struct(~[T_int(), T_i8()]);
    tn.associate(s, t);
    ret t;
}

fn T_opaque_tag_ptr(tn: &type_names) -> TypeRef {
    ret T_ptr(T_opaque_tag(tn));
}

fn T_captured_tydescs(cx: &crate_ctxt, n: uint) -> TypeRef {
    ret T_struct(std::ivec::init_elt[TypeRef](T_ptr(cx.tydesc_type), n));
}

fn T_obj_ptr(cx: &crate_ctxt, n_captured_tydescs: uint) -> TypeRef {
    // This function is not publicly exposed because it returns an incomplete
    // type. The dynamically-sized fields follow the captured tydescs.

    fn T_obj(cx: &crate_ctxt, n_captured_tydescs: uint) -> TypeRef {
        ret T_struct(~[T_ptr(cx.tydesc_type),
                       T_captured_tydescs(cx, n_captured_tydescs)]);
    }
    ret T_ptr(T_box(T_obj(cx, n_captured_tydescs)));
}

fn T_opaque_obj_ptr(cx: &crate_ctxt) -> TypeRef { ret T_obj_ptr(cx, 0u); }

fn T_opaque_port_ptr() -> TypeRef { ret T_ptr(T_i8()); }

fn T_opaque_chan_ptr() -> TypeRef { ret T_ptr(T_i8()); }


// LLVM constant constructors.
fn C_null(t: TypeRef) -> ValueRef { ret llvm::LLVMConstNull(t); }

fn C_integral(t: TypeRef, u: uint, sign_extend: Bool) -> ValueRef {
    // FIXME: We can't use LLVM::ULongLong with our existing minimal native
    // API, which only knows word-sized args.
    //
    // ret llvm::LLVMConstInt(T_int(), t as LLVM::ULongLong, False);
    //

    ret llvm::LLVMRustConstSmallInt(t, u, sign_extend);
}

fn C_float(s: &str) -> ValueRef {
    ret llvm::LLVMConstRealOfString(T_float(), str::buf(s));
}

fn C_floating(s: &str, t: TypeRef) -> ValueRef {
    ret llvm::LLVMConstRealOfString(t, str::buf(s));
}

fn C_nil() -> ValueRef {
    // NB: See comment above in T_void().

    ret C_integral(T_i1(), 0u, False);
}

fn C_bool(b: bool) -> ValueRef {
    if b {
        ret C_integral(T_bool(), 1u, False);
    } else { ret C_integral(T_bool(), 0u, False); }
}

fn C_int(i: int) -> ValueRef { ret C_integral(T_int(), i as uint, True); }

fn C_uint(i: uint) -> ValueRef { ret C_integral(T_int(), i, False); }

fn C_u8(i: uint) -> ValueRef { ret C_integral(T_i8(), i, False); }


// This is a 'c-like' raw string, which differs from
// our boxed-and-length-annotated strings.
fn C_cstr(cx: &@crate_ctxt, s: &str) -> ValueRef {
    let sc = llvm::LLVMConstString(str::buf(s), str::byte_len(s), False);
    let g =
        llvm::LLVMAddGlobal(cx.llmod, val_ty(sc),
                            str::buf(cx.names.next("str")));
    llvm::LLVMSetInitializer(g, sc);
    llvm::LLVMSetGlobalConstant(g, True);
    llvm::LLVMSetLinkage(g, lib::llvm::LLVMInternalLinkage as llvm::Linkage);
    ret g;
}


// A rust boxed-and-length-annotated string.
fn C_str(cx: &@crate_ctxt, s: &str) -> ValueRef {
    let len = str::byte_len(s);
    let  // 'alloc'
         // 'fill'
         // 'pad'
        box =
        C_struct(~[C_int(abi::const_refcount as int), C_int(len + 1u as int),
                   C_int(len + 1u as int), C_int(0),
                   llvm::LLVMConstString(str::buf(s), len, False)]);
    let g =
        llvm::LLVMAddGlobal(cx.llmod, val_ty(box),
                            str::buf(cx.names.next("str")));
    llvm::LLVMSetInitializer(g, box);
    llvm::LLVMSetGlobalConstant(g, True);
    llvm::LLVMSetLinkage(g, lib::llvm::LLVMInternalLinkage as llvm::Linkage);
    ret llvm::LLVMConstPointerCast(g, T_ptr(T_str()));
}

// Returns a Plain Old LLVM String:
fn C_postr(s: &str) -> ValueRef {
    ret llvm::LLVMConstString(str::buf(s), str::byte_len(s), False);
}

fn C_zero_byte_arr(size: uint) -> ValueRef {
    let i = 0u;
    let elts: ValueRef[] = ~[];
    while i < size { elts += ~[C_u8(0u)]; i += 1u; }
    ret llvm::LLVMConstArray(T_i8(), std::ivec::to_ptr(elts),
                             std::ivec::len(elts));
}

fn C_struct(elts: &ValueRef[]) -> ValueRef {
    ret llvm::LLVMConstStruct(std::ivec::to_ptr(elts), std::ivec::len(elts),
                              False);
}

fn C_named_struct(T: TypeRef, elts: &ValueRef[]) -> ValueRef {
    ret llvm::LLVMConstNamedStruct(T, std::ivec::to_ptr(elts),
                                   std::ivec::len(elts));
}

fn C_array(ty: TypeRef, elts: &ValueRef[]) -> ValueRef {
    ret llvm::LLVMConstArray(ty, std::ivec::to_ptr(elts),
                             std::ivec::len(elts));
}
