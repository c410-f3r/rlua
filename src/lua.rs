use std::{mem, ptr, str};
use std::sync::{Arc, Mutex};
use std::cell::RefCell;
use std::ffi::CString;
use std::any::{Any, TypeId};
use std::marker::PhantomData;
use std::collections::HashMap;
use std::os::raw::{c_char, c_int, c_void};

use libc;

use ffi;
use error::*;
use util::*;
use value::{FromLua, FromLuaMulti, MultiValue, Nil, ToLua, ToLuaMulti, Value};
use types::{Callback, Integer, LightUserData, LuaRef, Number, RegistryKey};
use string::String;
use table::Table;
use function::Function;
use thread::Thread;
use userdata::{AnyUserData, MetaMethod, UserData, UserDataMethods};

/// Top level Lua struct which holds the Lua state itself.
pub struct Lua {
    pub(crate) state: *mut ffi::lua_State,
    main_state: *mut ffi::lua_State,
    ephemeral: bool,
}

/// Constructed by the [`Lua::scope`] method, allows temporarily passing to Lua userdata that is
/// !Send, and callbacks that are !Send and not 'static.
///
/// See [`Lua::scope`] for more details.
///
/// [`Lua::scope`]: struct.Lua.html#method.scope
pub struct Scope<'scope> {
    lua: &'scope Lua,
    destructors: RefCell<Vec<Box<Fn(*mut ffi::lua_State) -> Box<Any>>>>,
    // 'scope lifetime must be invariant
    _scope: PhantomData<&'scope mut &'scope ()>,
}

// Data associated with the main lua_State via lua_getextraspace.
struct ExtraData {
    registered_userdata: HashMap<TypeId, c_int>,
    registry_unref_list: Arc<Mutex<Option<Vec<c_int>>>>,
}

unsafe impl Send for Lua {}

impl Drop for Lua {
    fn drop(&mut self) {
        unsafe {
            if !self.ephemeral {
                let top = ffi::lua_gettop(self.state);
                rlua_assert!(top == 0, "stack leak detected, stack top is {}", top);

                let extra_data = *(ffi::lua_getextraspace(self.state) as *mut *mut ExtraData);
                *(*extra_data).registry_unref_list.lock().unwrap() = None;
                Box::from_raw(extra_data);

                ffi::lua_close(self.state);
            }
        }
    }
}

impl Lua {
    /// Creates a new Lua state and loads standard library without the `debug` library.
    pub fn new() -> Lua {
        unsafe { Lua::create_lua(false) }
    }

    /// Creates a new Lua state and loads the standard library including the `debug` library.
    ///
    /// The debug library is very unsound, loading it and using it breaks all the guarantees of
    /// rlua.
    pub unsafe fn new_with_debug() -> Lua {
        Lua::create_lua(true)
    }

    /// Loads a chunk of Lua code and returns it as a function.
    ///
    /// The source can be named by setting the `name` parameter. This is generally recommended as it
    /// results in better error traces.
    ///
    /// Equivalent to Lua's `load` function.
    pub fn load(&self, source: &str, name: Option<&str>) -> Result<Function> {
        unsafe {
            stack_err_guard(self.state, || {
                check_stack(self.state, 1);

                match if let Some(name) = name {
                    let name =
                        CString::new(name.to_owned()).map_err(|e| Error::ToLuaConversionError {
                            from: "&str",
                            to: "string",
                            message: Some(e.to_string()),
                        })?;
                    ffi::luaL_loadbuffer(
                        self.state,
                        source.as_ptr() as *const c_char,
                        source.len(),
                        name.as_ptr(),
                    )
                } else {
                    ffi::luaL_loadbuffer(
                        self.state,
                        source.as_ptr() as *const c_char,
                        source.len(),
                        ptr::null(),
                    )
                } {
                    ffi::LUA_OK => Ok(Function(self.pop_ref(self.state))),
                    err => Err(pop_error(self.state, err)),
                }
            })
        }
    }

    /// Execute a chunk of Lua code.
    ///
    /// This is equivalent to simply loading the source with `load` and then calling the resulting
    /// function with no arguments.
    ///
    /// Returns the values returned by the chunk.
    pub fn exec<'lua, R: FromLuaMulti<'lua>>(
        &'lua self,
        source: &str,
        name: Option<&str>,
    ) -> Result<R> {
        self.load(source, name)?.call(())
    }

    /// Evaluate the given expression or chunk inside this Lua state.
    ///
    /// If `source` is an expression, returns the value it evaluates to. Otherwise, returns the
    /// values returned by the chunk (if any).
    pub fn eval<'lua, R: FromLuaMulti<'lua>>(
        &'lua self,
        source: &str,
        name: Option<&str>,
    ) -> Result<R> {
        // First, try interpreting the lua as an expression by adding
        // "return", then as a statement.  This is the same thing the
        // actual lua repl does.
        self.load(&format!("return {}", source), name)
            .or_else(|_| self.load(source, name))?
            .call(())
    }

    /// Pass a `&str` slice to Lua, creating and returning an interned Lua string.
    pub fn create_string(&self, s: &str) -> Result<String> {
        unsafe {
            stack_err_guard(self.state, || {
                check_stack(self.state, 4);
                push_string(self.state, s)?;
                Ok(String(self.pop_ref(self.state)))
            })
        }
    }

    /// Creates and returns a new table.
    pub fn create_table(&self) -> Result<Table> {
        unsafe {
            stack_err_guard(self.state, || {
                check_stack(self.state, 4);
                protect_lua_call(self.state, 0, 1, |state| {
                    ffi::lua_newtable(state);
                })?;
                Ok(Table(self.pop_ref(self.state)))
            })
        }
    }

    /// Creates a table and fills it with values from an iterator.
    pub fn create_table_from<'lua, K, V, I>(&'lua self, cont: I) -> Result<Table<'lua>>
    where
        K: ToLua<'lua>,
        V: ToLua<'lua>,
        I: IntoIterator<Item = (K, V)>,
    {
        unsafe {
            stack_err_guard(self.state, || {
                check_stack(self.state, 6);
                protect_lua_call(self.state, 0, 1, |state| {
                    ffi::lua_newtable(state);
                })?;

                for (k, v) in cont {
                    self.push_value(self.state, k.to_lua(self)?);
                    self.push_value(self.state, v.to_lua(self)?);
                    protect_lua_call(self.state, 3, 1, |state| {
                        ffi::lua_rawset(state, -3);
                    })?;
                }
                Ok(Table(self.pop_ref(self.state)))
            })
        }
    }

    /// Creates a table from an iterator of values, using `1..` as the keys.
    pub fn create_sequence_from<'lua, T, I>(&'lua self, cont: I) -> Result<Table<'lua>>
    where
        T: ToLua<'lua>,
        I: IntoIterator<Item = T>,
    {
        self.create_table_from(cont.into_iter().enumerate().map(|(k, v)| (k + 1, v)))
    }

    /// Wraps a Rust function or closure, creating a callable Lua function handle to it.
    ///
    /// The function's return value is always a `Result`: If the function returns `Err`, the error
    /// is raised as a Lua error, which can be caught using `(x)pcall` or bubble up to the Rust code
    /// that invoked the Lua code. This allows using the `?` operator to propagate errors through
    /// intermediate Lua code.
    ///
    /// If the function returns `Ok`, the contained value will be converted to one or more Lua
    /// values. For details on Rust-to-Lua conversions, refer to the [`ToLua`] and [`ToLuaMulti`]
    /// traits.
    ///
    /// # Examples
    ///
    /// Create a function which prints its argument:
    ///
    /// ```
    /// # extern crate rlua;
    /// # use rlua::{Lua, Result};
    /// # fn try_main() -> Result<()> {
    /// let lua = Lua::new();
    ///
    /// let greet = lua.create_function(|_, name: String| {
    ///     println!("Hello, {}!", name);
    ///     Ok(())
    /// });
    /// # let _ = greet;    // used
    /// # Ok(())
    /// # }
    /// # fn main() {
    /// #     try_main().unwrap();
    /// # }
    /// ```
    ///
    /// Use tuples to accept multiple arguments:
    ///
    /// ```
    /// # extern crate rlua;
    /// # use rlua::{Lua, Result};
    /// # fn try_main() -> Result<()> {
    /// let lua = Lua::new();
    ///
    /// let print_person = lua.create_function(|_, (name, age): (String, u8)| {
    ///     println!("{} is {} years old!", name, age);
    ///     Ok(())
    /// });
    /// # let _ = print_person;    // used
    /// # Ok(())
    /// # }
    /// # fn main() {
    /// #     try_main().unwrap();
    /// # }
    /// ```
    ///
    /// [`ToLua`]: trait.ToLua.html
    /// [`ToLuaMulti`]: trait.ToLuaMulti.html
    pub fn create_function<'lua, 'callback, A, R, F>(&'lua self, func: F) -> Result<Function<'lua>>
    where
        A: FromLuaMulti<'callback>,
        R: ToLuaMulti<'callback>,
        F: 'static + Send + Fn(&'callback Lua, A) -> Result<R>,
    {
        self.create_callback_function(Box::new(move |lua, args| {
            func(lua, A::from_lua_multi(args, lua)?)?.to_lua_multi(lua)
        }))
    }

    /// Wraps a Rust mutable closure, creating a callable Lua function handle to it.
    ///
    /// This is a version of [`create_function`] that accepts a FnMut argument.  Refer to
    /// [`create_function`] for more information about the implementation.
    ///
    /// [`create_function`]: #method.create_function
    pub fn create_function_mut<'lua, 'callback, A, R, F>(
        &'lua self,
        func: F,
    ) -> Result<Function<'lua>>
    where
        A: FromLuaMulti<'callback>,
        R: ToLuaMulti<'callback>,
        F: 'static + Send + FnMut(&'callback Lua, A) -> Result<R>,
    {
        let func = RefCell::new(func);
        self.create_function(move |lua, args| {
            (&mut *func.try_borrow_mut()
                .map_err(|_| Error::RecursiveMutCallback)?)(lua, args)
        })
    }

    /// Wraps a Lua function into a new thread (or coroutine).
    ///
    /// Equivalent to `coroutine.create`.
    pub fn create_thread<'lua>(&'lua self, func: Function<'lua>) -> Result<Thread<'lua>> {
        unsafe {
            stack_err_guard(self.state, move || {
                check_stack(self.state, 2);

                let thread_state =
                    protect_lua_call(self.state, 0, 1, |state| ffi::lua_newthread(state))?;
                self.push_ref(thread_state, &func.0);

                Ok(Thread(self.pop_ref(self.state)))
            })
        }
    }

    /// Create a Lua userdata object from a custom userdata type.
    pub fn create_userdata<T>(&self, data: T) -> Result<AnyUserData>
    where
        T: Send + UserData,
    {
        self.do_create_userdata(data)
    }

    /// Returns a handle to the global environment.
    pub fn globals(&self) -> Table {
        unsafe {
            stack_guard(self.state, move || {
                check_stack(self.state, 2);
                ffi::lua_rawgeti(self.state, ffi::LUA_REGISTRYINDEX, ffi::LUA_RIDX_GLOBALS);
                Table(self.pop_ref(self.state))
            })
        }
    }

    /// Calls the given function with a `Scope` parameter, giving the function the ability to create
    /// userdata from rust types that are !Send, and rust callbacks that are !Send and not 'static.
    ///
    /// The lifetime of any function or userdata created through `Scope` lasts only until the
    /// completion of this method call, on completion all such created values are automatically
    /// dropped and Lua references to them are invalidated.  If a script accesses a value created
    /// through `Scope` outside of this method, a Lua error will result.  Since we can ensure the
    /// lifetime of values created through `Scope`, and we know that `Lua` cannot be sent to another
    /// thread while `Scope` is live, it is safe to allow !Send datatypes and functions whose
    /// lifetimes only outlive the scope lifetime.
    ///
    /// Handles that `Lua::scope` produces have a `'lua` lifetime of the scope parameter, to prevent
    /// the handles from escaping the callback.  However, this is not the only way for values to
    /// escape the callback, as they can be smuggled through Lua itself.  This is safe to do, but
    /// not very useful, because after the scope is dropped, all references to scoped values,
    /// whether in Lua or in rust, are invalidated.  `Function` types will error when called, and
    /// `AnyUserData` types will be typeless.
    pub fn scope<'scope, 'lua: 'scope, F, R>(&'lua self, f: F) -> R
    where
        F: FnOnce(&Scope<'scope>) -> R,
    {
        let scope = Scope {
            lua: self,
            destructors: RefCell::new(Vec::new()),
            _scope: PhantomData,
        };
        let r = f(&scope);
        drop(scope);
        r
    }

    /// Coerces a Lua value to a string.
    ///
    /// The value must be a string (in which case this is a no-op) or a number.
    pub fn coerce_string<'lua>(&'lua self, v: Value<'lua>) -> Result<String<'lua>> {
        match v {
            Value::String(s) => Ok(s),
            v => unsafe {
                stack_err_guard(self.state, || {
                    check_stack(self.state, 4);
                    let ty = v.type_name();
                    self.push_value(self.state, v);
                    let s =
                        protect_lua_call(self.state, 1, 1, |state| ffi::lua_tostring(state, -1))?;
                    if s.is_null() {
                        ffi::lua_pop(self.state, 1);
                        Err(Error::FromLuaConversionError {
                            from: ty,
                            to: "String",
                            message: Some("expected string or number".to_string()),
                        })
                    } else {
                        Ok(String(self.pop_ref(self.state)))
                    }
                })
            },
        }
    }

    /// Coerces a Lua value to an integer.
    ///
    /// The value must be an integer, or a floating point number or a string that can be converted
    /// to an integer. Refer to the Lua manual for details.
    pub fn coerce_integer(&self, v: Value) -> Result<Integer> {
        match v {
            Value::Integer(i) => Ok(i),
            v => unsafe {
                stack_guard(self.state, || {
                    check_stack(self.state, 2);
                    let ty = v.type_name();
                    self.push_value(self.state, v);
                    let mut isint = 0;
                    let i = ffi::lua_tointegerx(self.state, -1, &mut isint);
                    ffi::lua_pop(self.state, 1);
                    if isint == 0 {
                        Err(Error::FromLuaConversionError {
                            from: ty,
                            to: "integer",
                            message: None,
                        })
                    } else {
                        Ok(i)
                    }
                })
            },
        }
    }

    /// Coerce a Lua value to a number.
    ///
    /// The value must be a number or a string that can be converted to a number. Refer to the Lua
    /// manual for details.
    pub fn coerce_number(&self, v: Value) -> Result<Number> {
        match v {
            Value::Number(n) => Ok(n),
            v => unsafe {
                stack_guard(self.state, || {
                    check_stack(self.state, 2);
                    let ty = v.type_name();
                    self.push_value(self.state, v);
                    let mut isnum = 0;
                    let n = ffi::lua_tonumberx(self.state, -1, &mut isnum);
                    ffi::lua_pop(self.state, 1);
                    if isnum == 0 {
                        Err(Error::FromLuaConversionError {
                            from: ty,
                            to: "number",
                            message: Some("number or string coercible to number".to_string()),
                        })
                    } else {
                        Ok(n)
                    }
                })
            },
        }
    }

    /// Converts a value that implements `ToLua` into a `Value` instance.
    pub fn pack<'lua, T: ToLua<'lua>>(&'lua self, t: T) -> Result<Value<'lua>> {
        t.to_lua(self)
    }

    /// Converts a `Value` instance into a value that implements `FromLua`.
    pub fn unpack<'lua, T: FromLua<'lua>>(&'lua self, value: Value<'lua>) -> Result<T> {
        T::from_lua(value, self)
    }

    /// Converts a value that implements `ToLuaMulti` into a `MultiValue` instance.
    pub fn pack_multi<'lua, T: ToLuaMulti<'lua>>(&'lua self, t: T) -> Result<MultiValue<'lua>> {
        t.to_lua_multi(self)
    }

    /// Converts a `MultiValue` instance into a value that implements `FromLuaMulti`.
    pub fn unpack_multi<'lua, T: FromLuaMulti<'lua>>(
        &'lua self,
        value: MultiValue<'lua>,
    ) -> Result<T> {
        T::from_lua_multi(value, self)
    }

    /// Set a value in the Lua registry based on a string name.
    ///
    /// This value will be available to rust from all `Lua` instances which share the same main
    /// state.
    pub fn set_named_registry_value<'lua, T: ToLua<'lua>>(
        &'lua self,
        name: &str,
        t: T,
    ) -> Result<()> {
        unsafe {
            stack_err_guard(self.state, || {
                check_stack(self.state, 5);

                push_string(self.state, name)?;
                self.push_value(self.state, t.to_lua(self)?);

                protect_lua_call(self.state, 2, 0, |state| {
                    ffi::lua_rawset(state, ffi::LUA_REGISTRYINDEX);
                })
            })
        }
    }

    /// Get a value from the Lua registry based on a string name.
    ///
    /// Any Lua instance which shares the underlying main state may call this method to
    /// get a value previously set by [`set_named_registry_value`].
    ///
    /// [`set_named_registry_value`]: #method.set_named_registry_value
    pub fn named_registry_value<'lua, T: FromLua<'lua>>(&'lua self, name: &str) -> Result<T> {
        unsafe {
            stack_err_guard(self.state, || {
                check_stack(self.state, 4);

                push_string(self.state, name)?;
                protect_lua_call(self.state, 1, 1, |state| {
                    ffi::lua_rawget(state, ffi::LUA_REGISTRYINDEX)
                })?;

                T::from_lua(self.pop_value(self.state), self)
            })
        }
    }

    /// Removes a named value in the Lua registry.
    ///
    /// Equivalent to calling [`set_named_registry_value`] with a value of Nil.
    ///
    /// [`set_named_registry_value`]: #method.set_named_registry_value
    pub fn unset_named_registry_value<'lua>(&'lua self, name: &str) -> Result<()> {
        self.set_named_registry_value(name, Nil)
    }

    /// Place a value in the Lua registry with an auto-generated key.
    ///
    /// This value will be available to rust from all `Lua` instances which share the same main
    /// state.
    pub fn create_registry_value<'lua, T: ToLua<'lua>>(&'lua self, t: T) -> Result<RegistryKey> {
        unsafe {
            stack_guard(self.state, || {
                check_stack(self.state, 2);

                self.push_value(self.state, t.to_lua(self)?);
                let registry_id = gc_guard(self.state, || {
                    ffi::luaL_ref(self.state, ffi::LUA_REGISTRYINDEX)
                });

                Ok(RegistryKey {
                    registry_id,
                    unref_list: (*self.extra()).registry_unref_list.clone(),
                    drop_unref: true,
                })
            })
        }
    }

    /// Get a value from the Lua registry by its `RegistryKey`
    ///
    /// Any Lua instance which shares the underlying main state may call this method to get a value
    /// previously placed by [`create_registry_value`].
    ///
    /// [`create_registry_value`]: #method.create_registry_value
    pub fn registry_value<'lua, T: FromLua<'lua>>(&'lua self, key: &RegistryKey) -> Result<T> {
        unsafe {
            if !Arc::ptr_eq(&key.unref_list, &(*self.extra()).registry_unref_list) {
                return Err(Error::MismatchedRegistryKey);
            }

            stack_err_guard(self.state, || {
                check_stack(self.state, 2);
                ffi::lua_rawgeti(
                    self.state,
                    ffi::LUA_REGISTRYINDEX,
                    key.registry_id as ffi::lua_Integer,
                );
                T::from_lua(self.pop_value(self.state), self)
            })
        }
    }

    /// Removes a value from the Lua registry.
    ///
    /// You may call this function to manually remove a value placed in the registry with
    /// [`create_registry_value`]. In addition to manual `RegistryKey` removal, you can also call
    /// [`expire_registry_values`] to automatically remove values from the registry whose
    /// `RegistryKey`s have been dropped.
    ///
    /// [`create_registry_value`]: #method.create_registry_value
    /// [`expire_registry_values`]: #method.expire_registry_values
    pub fn remove_registry_value(&self, mut key: RegistryKey) -> Result<()> {
        unsafe {
            if !Arc::ptr_eq(&key.unref_list, &(*self.extra()).registry_unref_list) {
                return Err(Error::MismatchedRegistryKey);
            }

            ffi::luaL_unref(self.state, ffi::LUA_REGISTRYINDEX, key.registry_id);
            key.drop_unref = false;
            Ok(())
        }
    }

    /// Returns true if the given `RegistryKey` was created by a `Lua` which shares the underlying
    /// main state with this `Lua` instance.
    ///
    /// Other than this, methods that accept a `RegistryKey` will return
    /// `Error::MismatchedRegistryKey` if passed a `RegistryKey` that was not created with a
    /// matching `Lua` state.
    pub fn owns_registry_value(&self, key: &RegistryKey) -> bool {
        unsafe { Arc::ptr_eq(&key.unref_list, &(*self.extra()).registry_unref_list) }
    }

    /// Remove any registry values whose `RegistryKey`s have all been dropped.
    ///
    /// Unlike normal handle values, `RegistryKey`s cannot automatically clean up their registry
    /// entries on Drop, but you can call this method to remove any unreachable registry values.
    pub fn expire_registry_values(&self) {
        unsafe {
            let unref_list = mem::replace(
                &mut *(*self.extra()).registry_unref_list.lock().unwrap(),
                Some(Vec::new()),
            );
            for id in unref_list.unwrap() {
                ffi::luaL_unref(self.state, ffi::LUA_REGISTRYINDEX, id);
            }
        }
    }

    // Uses 2 stack spaces, does not call checkstack
    pub(crate) unsafe fn push_value(&self, state: *mut ffi::lua_State, value: Value) {
        match value {
            Value::Nil => {
                ffi::lua_pushnil(state);
            }

            Value::Boolean(b) => {
                ffi::lua_pushboolean(state, if b { 1 } else { 0 });
            }

            Value::LightUserData(ud) => {
                ffi::lua_pushlightuserdata(state, ud.0);
            }

            Value::Integer(i) => {
                ffi::lua_pushinteger(state, i);
            }

            Value::Number(n) => {
                ffi::lua_pushnumber(state, n);
            }

            Value::String(s) => {
                self.push_ref(state, &s.0);
            }

            Value::Table(t) => {
                self.push_ref(state, &t.0);
            }

            Value::Function(f) => {
                self.push_ref(state, &f.0);
            }

            Value::Thread(t) => {
                self.push_ref(state, &t.0);
            }

            Value::UserData(ud) => {
                self.push_ref(state, &ud.0);
            }

            Value::Error(e) => {
                push_wrapped_error(state, e);
            }
        }
    }

    // Uses 2 stack spaces, does not call checkstack
    pub(crate) unsafe fn pop_value(&self, state: *mut ffi::lua_State) -> Value {
        match ffi::lua_type(state, -1) {
            ffi::LUA_TNIL => {
                ffi::lua_pop(state, 1);
                Nil
            }

            ffi::LUA_TBOOLEAN => {
                let b = Value::Boolean(ffi::lua_toboolean(state, -1) != 0);
                ffi::lua_pop(state, 1);
                b
            }

            ffi::LUA_TLIGHTUSERDATA => {
                let ud = Value::LightUserData(LightUserData(ffi::lua_touserdata(state, -1)));
                ffi::lua_pop(state, 1);
                ud
            }

            ffi::LUA_TNUMBER => if ffi::lua_isinteger(state, -1) != 0 {
                let i = Value::Integer(ffi::lua_tointeger(state, -1));
                ffi::lua_pop(state, 1);
                i
            } else {
                let n = Value::Number(ffi::lua_tonumber(state, -1));
                ffi::lua_pop(state, 1);
                n
            },

            ffi::LUA_TSTRING => Value::String(String(self.pop_ref(state))),

            ffi::LUA_TTABLE => Value::Table(Table(self.pop_ref(state))),

            ffi::LUA_TFUNCTION => Value::Function(Function(self.pop_ref(state))),

            ffi::LUA_TUSERDATA => {
                // It should not be possible to interact with userdata types other than custom
                // UserData types OR a WrappedError.  WrappedPanic should never be able to be caught
                // in lua, so it should never be here.
                if let Some(err) = pop_wrapped_error(state) {
                    Value::Error(err)
                } else {
                    Value::UserData(AnyUserData(self.pop_ref(state)))
                }
            }

            ffi::LUA_TTHREAD => Value::Thread(Thread(self.pop_ref(state))),

            _ => unreachable!("internal error: LUA_TNONE in pop_value"),
        }
    }

    // Used 1 stack space, does not call checkstack
    pub(crate) unsafe fn push_ref(&self, state: *mut ffi::lua_State, lref: &LuaRef) {
        assert!(
            lref.lua.main_state == self.main_state,
            "Lua instance passed Value created from a different Lua"
        );

        ffi::lua_rawgeti(
            state,
            ffi::LUA_REGISTRYINDEX,
            lref.registry_id as ffi::lua_Integer,
        );
    }

    // Pops the topmost element of the stack and stores a reference to it in the
    // registry.
    //
    // This pins the object, preventing garbage collection until the returned
    // `LuaRef` is dropped.
    //
    // pop_ref uses 1 extra stack space and does not call checkstack
    pub(crate) unsafe fn pop_ref(&self, state: *mut ffi::lua_State) -> LuaRef {
        let registry_id = gc_guard(state, || ffi::luaL_ref(state, ffi::LUA_REGISTRYINDEX));
        LuaRef {
            lua: self,
            registry_id: registry_id,
            drop_unref: true,
        }
    }

    pub(crate) unsafe fn userdata_metatable<T: UserData>(&self) -> Result<c_int> {
        // Used if both an __index metamethod is set and regular methods, checks methods table
        // first, then __index metamethod.
        unsafe extern "C" fn meta_index_impl(state: *mut ffi::lua_State) -> c_int {
            ffi::luaL_checkstack(state, 2, ptr::null());

            ffi::lua_pushvalue(state, -1);
            ffi::lua_gettable(state, ffi::lua_upvalueindex(1));
            if ffi::lua_isnil(state, -1) == 0 {
                ffi::lua_insert(state, -3);
                ffi::lua_pop(state, 2);
                1
            } else {
                ffi::lua_pop(state, 1);
                ffi::lua_pushvalue(state, ffi::lua_upvalueindex(2));
                ffi::lua_insert(state, -3);
                ffi::lua_call(state, 2, 1);
                1
            }
        }

        stack_err_guard(self.state, move || {
            check_stack(self.state, 5);

            if let Some(table_id) = (*self.extra()).registered_userdata.get(&TypeId::of::<T>()) {
                return Ok(*table_id);
            }

            let mut methods = UserDataMethods {
                methods: HashMap::new(),
                meta_methods: HashMap::new(),
                _type: PhantomData,
            };
            T::add_methods(&mut methods);

            protect_lua_call(self.state, 0, 1, |state| {
                ffi::lua_newtable(state);
            })?;

            let has_methods = !methods.methods.is_empty();

            if has_methods {
                push_string(self.state, "__index")?;
                protect_lua_call(self.state, 0, 1, |state| {
                    ffi::lua_newtable(state);
                })?;

                for (k, m) in methods.methods {
                    push_string(self.state, &k)?;
                    self.push_value(
                        self.state,
                        Value::Function(self.create_callback_function(m)?),
                    );
                    protect_lua_call(self.state, 3, 1, |state| {
                        ffi::lua_rawset(state, -3);
                    })?;
                }

                protect_lua_call(self.state, 3, 1, |state| {
                    ffi::lua_rawset(state, -3);
                })?;
            }

            for (k, m) in methods.meta_methods {
                if k == MetaMethod::Index && has_methods {
                    push_string(self.state, "__index")?;
                    ffi::lua_pushvalue(self.state, -1);
                    ffi::lua_gettable(self.state, -3);
                    self.push_value(
                        self.state,
                        Value::Function(self.create_callback_function(m)?),
                    );
                    protect_lua_call(self.state, 2, 1, |state| {
                        ffi::lua_pushcclosure(state, meta_index_impl, 2);
                    })?;

                    protect_lua_call(self.state, 3, 1, |state| {
                        ffi::lua_rawset(state, -3);
                    })?;
                } else {
                    let name = match k {
                        MetaMethod::Add => "__add",
                        MetaMethod::Sub => "__sub",
                        MetaMethod::Mul => "__mul",
                        MetaMethod::Div => "__div",
                        MetaMethod::Mod => "__mod",
                        MetaMethod::Pow => "__pow",
                        MetaMethod::Unm => "__unm",
                        MetaMethod::IDiv => "__idiv",
                        MetaMethod::BAnd => "__band",
                        MetaMethod::BOr => "__bor",
                        MetaMethod::BXor => "__bxor",
                        MetaMethod::BNot => "__bnot",
                        MetaMethod::Shl => "__shl",
                        MetaMethod::Shr => "__shr",
                        MetaMethod::Concat => "__concat",
                        MetaMethod::Len => "__len",
                        MetaMethod::Eq => "__eq",
                        MetaMethod::Lt => "__lt",
                        MetaMethod::Le => "__le",
                        MetaMethod::Index => "__index",
                        MetaMethod::NewIndex => "__newindex",
                        MetaMethod::Call => "__call",
                        MetaMethod::ToString => "__tostring",
                    };
                    push_string(self.state, name)?;
                    self.push_value(
                        self.state,
                        Value::Function(self.create_callback_function(m)?),
                    );
                    protect_lua_call(self.state, 3, 1, |state| {
                        ffi::lua_rawset(state, -3);
                    })?;
                }
            }

            push_string(self.state, "__gc")?;
            ffi::lua_pushcfunction(self.state, userdata_destructor::<RefCell<T>>);
            protect_lua_call(self.state, 3, 1, |state| {
                ffi::lua_rawset(state, -3);
            })?;

            push_string(self.state, "__metatable")?;
            ffi::lua_pushboolean(self.state, 0);
            protect_lua_call(self.state, 3, 1, |state| {
                ffi::lua_rawset(state, -3);
            })?;

            let id = gc_guard(self.state, || {
                ffi::luaL_ref(self.state, ffi::LUA_REGISTRYINDEX)
            });
            (*self.extra())
                .registered_userdata
                .insert(TypeId::of::<T>(), id);
            Ok(id)
        })
    }

    unsafe fn create_lua(load_debug: bool) -> Lua {
        unsafe extern "C" fn allocator(
            _: *mut c_void,
            ptr: *mut c_void,
            _: usize,
            nsize: usize,
        ) -> *mut c_void {
            if nsize == 0 {
                libc::free(ptr as *mut libc::c_void);
                ptr::null_mut()
            } else {
                let p = libc::realloc(ptr as *mut libc::c_void, nsize);
                if p.is_null() {
                    // We require that OOM results in an abort, and that the lua allocator function
                    // never errors.  Since this is what rust itself normally does on OOM, this is
                    // not really a huge loss.  Importantly, this allows us to turn off the gc, and
                    // then know that calling Lua API functions marked as 'm' will not result in a
                    // 'longjmp' error while the gc is off.
                    abort!("out of memory in Lua allocation, aborting!");
                } else {
                    p as *mut c_void
                }
            }
        }

        let state = ffi::lua_newstate(allocator, ptr::null_mut());

        // Ignores or `unwrap()`s 'm' errors, because this is assuming that nothing in the lua
        // standard library will have a `__gc` metamethod error.
        stack_guard(state, || {
            // Do not open the debug library, it can be used to cause unsafety.
            ffi::luaL_requiref(state, cstr!("_G"), ffi::luaopen_base, 1);
            ffi::luaL_requiref(state, cstr!("coroutine"), ffi::luaopen_coroutine, 1);
            ffi::luaL_requiref(state, cstr!("table"), ffi::luaopen_table, 1);
            ffi::luaL_requiref(state, cstr!("io"), ffi::luaopen_io, 1);
            ffi::luaL_requiref(state, cstr!("os"), ffi::luaopen_os, 1);
            ffi::luaL_requiref(state, cstr!("string"), ffi::luaopen_string, 1);
            ffi::luaL_requiref(state, cstr!("utf8"), ffi::luaopen_utf8, 1);
            ffi::luaL_requiref(state, cstr!("math"), ffi::luaopen_math, 1);
            ffi::luaL_requiref(state, cstr!("package"), ffi::luaopen_package, 1);
            ffi::lua_pop(state, 9);

            init_error_metatables(state);

            if load_debug {
                ffi::luaL_requiref(state, cstr!("debug"), ffi::luaopen_debug, 1);
                ffi::lua_pop(state, 1);
            }

            // Create the function metatable

            ffi::lua_pushlightuserdata(
                state,
                &FUNCTION_METATABLE_REGISTRY_KEY as *const u8 as *mut c_void,
            );

            ffi::lua_newtable(state);

            push_string(state, "__gc").unwrap();
            ffi::lua_pushcfunction(state, userdata_destructor::<Callback>);
            ffi::lua_rawset(state, -3);

            push_string(state, "__metatable").unwrap();
            ffi::lua_pushboolean(state, 0);
            ffi::lua_rawset(state, -3);

            ffi::lua_rawset(state, ffi::LUA_REGISTRYINDEX);

            // Override pcall and xpcall with versions that cannot be used to catch rust panics.

            ffi::lua_rawgeti(state, ffi::LUA_REGISTRYINDEX, ffi::LUA_RIDX_GLOBALS);

            push_string(state, "pcall").unwrap();
            ffi::lua_pushcfunction(state, safe_pcall);
            ffi::lua_rawset(state, -3);

            push_string(state, "xpcall").unwrap();
            ffi::lua_pushcfunction(state, safe_xpcall);
            ffi::lua_rawset(state, -3);

            ffi::lua_pop(state, 1);

            // Create ExtraData, and place it in the lua_State "extra space"

            let extra_data = Box::into_raw(Box::new(ExtraData {
                registered_userdata: HashMap::new(),
                registry_unref_list: Arc::new(Mutex::new(Some(Vec::new()))),
            }));
            *(ffi::lua_getextraspace(state) as *mut *mut ExtraData) = extra_data;
        });

        Lua {
            state,
            main_state: state,
            ephemeral: false,
        }
    }

    fn create_callback_function<'lua, 'callback>(
        &'lua self,
        func: Callback<'callback, 'static>,
    ) -> Result<Function<'lua>> {
        unsafe extern "C" fn callback_call_impl(state: *mut ffi::lua_State) -> c_int {
            callback_error(state, || {
                let lua = Lua {
                    state: state,
                    main_state: main_state(state),
                    ephemeral: true,
                };

                if ffi::lua_type(state, ffi::lua_upvalueindex(1)) == ffi::LUA_TNIL {
                    return Err(Error::CallbackDestructed);
                }

                let func = get_userdata::<Callback>(state, ffi::lua_upvalueindex(1));

                let nargs = ffi::lua_gettop(state);
                let mut args = MultiValue::new();
                check_stack(state, 2);
                for _ in 0..nargs {
                    args.push_front(lua.pop_value(state));
                }

                let results = (*func)(&lua, args)?;
                let nresults = results.len() as c_int;

                check_stack_err(state, nresults)?;

                for r in results {
                    lua.push_value(state, r);
                }

                Ok(nresults)
            })
        }

        unsafe {
            stack_err_guard(self.state, move || {
                check_stack(self.state, 2);

                push_userdata::<Callback>(self.state, func)?;

                ffi::lua_pushlightuserdata(
                    self.state,
                    &FUNCTION_METATABLE_REGISTRY_KEY as *const u8 as *mut c_void,
                );
                ffi::lua_rawget(self.state, ffi::LUA_REGISTRYINDEX);
                ffi::lua_setmetatable(self.state, -2);

                protect_lua_call(self.state, 1, 1, |state| {
                    ffi::lua_pushcclosure(state, callback_call_impl, 1);
                })?;

                Ok(Function(self.pop_ref(self.state)))
            })
        }
    }

    fn do_create_userdata<T>(&self, data: T) -> Result<AnyUserData>
    where
        T: UserData,
    {
        unsafe {
            stack_err_guard(self.state, move || {
                check_stack(self.state, 3);

                push_userdata::<RefCell<T>>(self.state, RefCell::new(data))?;

                ffi::lua_rawgeti(
                    self.state,
                    ffi::LUA_REGISTRYINDEX,
                    self.userdata_metatable::<T>()? as ffi::lua_Integer,
                );

                ffi::lua_setmetatable(self.state, -2);

                Ok(AnyUserData(self.pop_ref(self.state)))
            })
        }
    }

    unsafe fn extra(&self) -> *mut ExtraData {
        *(ffi::lua_getextraspace(self.main_state) as *mut *mut ExtraData)
    }
}

impl<'scope> Scope<'scope> {
    /// Wraps a Rust function or closure, creating a callable Lua function handle to it.
    ///
    /// This is a version of [`Lua::create_function`] that creates a callback which expires on scope
    /// drop.  See [`Lua::scope`] for more details.
    ///
    /// [`Lua::create_function`]: struct.Lua.html#method.create_function
    /// [`Lua::scope`]: struct.Lua.html#method.scope
    pub fn create_function<'callback, 'lua, A, R, F>(&'lua self, func: F) -> Result<Function<'lua>>
    where
        A: FromLuaMulti<'callback>,
        R: ToLuaMulti<'callback>,
        F: 'scope + Fn(&'callback Lua, A) -> Result<R>,
    {
        unsafe {
            let f = Box::new(move |lua, args| {
                func(lua, A::from_lua_multi(args, lua)?)?.to_lua_multi(lua)
            });
            let f = mem::transmute::<Callback<'callback, 'scope>, Callback<'callback, 'static>>(f);
            let mut f = self.lua.create_callback_function(f)?;

            f.0.drop_unref = false;
            let mut destructors = self.destructors.borrow_mut();
            let registry_id = f.0.registry_id;
            destructors.push(Box::new(move |state| {
                stack_guard(state, || {
                    check_stack(state, 2);

                    ffi::lua_rawgeti(
                        state,
                        ffi::LUA_REGISTRYINDEX,
                        registry_id as ffi::lua_Integer,
                    );
                    ffi::luaL_unref(state, ffi::LUA_REGISTRYINDEX, registry_id);

                    ffi::lua_getupvalue(state, -1, 1);
                    let ud = take_userdata::<Callback>(state);

                    ffi::lua_pushnil(state);
                    ffi::lua_setupvalue(state, -2, 1);

                    ffi::lua_pop(state, 1);
                    Box::new(ud)
                })
            }));
            Ok(f)
        }
    }

    /// Wraps a Rust mutable closure, creating a callable Lua function handle to it.
    ///
    /// This is a version of [`Lua::create_function_mut`] that creates a callback which expires on
    /// scope drop.  See [`Lua::scope`] for more details.
    ///
    /// [`Lua::create_function_mut`]: struct.Lua.html#method.create_function_mut
    /// [`Lua::scope`]: struct.Lua.html#method.scope
    pub fn create_function_mut<'callback, 'lua, A, R, F>(
        &'lua self,
        func: F,
    ) -> Result<Function<'lua>>
    where
        A: FromLuaMulti<'callback>,
        R: ToLuaMulti<'callback>,
        F: 'scope + FnMut(&'callback Lua, A) -> Result<R>,
    {
        let func = RefCell::new(func);
        self.create_function(move |lua, args| {
            (&mut *func.try_borrow_mut()
                .map_err(|_| Error::RecursiveMutCallback)?)(lua, args)
        })
    }

    /// Create a Lua userdata object from a custom userdata type.
    ///
    /// This is a version of [`Lua::create_userdata`] that creates a userdata which expires on scope
    /// drop.  See [`Lua::scope`] for more details.
    ///
    /// [`Lua::create_userdata`]: struct.Lua.html#method.create_userdata
    /// [`Lua::scope`]: struct.Lua.html#method.scope
    pub fn create_userdata<'lua, T>(&'lua self, data: T) -> Result<AnyUserData<'lua>>
    where
        T: UserData,
    {
        unsafe {
            let mut u = self.lua.do_create_userdata(data)?;
            u.0.drop_unref = false;
            let mut destructors = self.destructors.borrow_mut();
            let registry_id = u.0.registry_id;
            destructors.push(Box::new(move |state| {
                stack_guard(state, || {
                    check_stack(state, 1);
                    ffi::lua_rawgeti(
                        state,
                        ffi::LUA_REGISTRYINDEX,
                        registry_id as ffi::lua_Integer,
                    );
                    ffi::luaL_unref(state, ffi::LUA_REGISTRYINDEX, registry_id);
                    Box::new(take_userdata::<RefCell<T>>(state))
                })
            }));
            Ok(u)
        }
    }
}

impl<'scope> Drop for Scope<'scope> {
    fn drop(&mut self) {
        // We separate the action of invalidating the userdata in Lua and actually dropping the
        // userdata type into two phases.  This is so that, in the event a userdata drop panics, we
        // can be sure that all of the userdata in Lua is actually invalidated.

        let state = self.lua.state;
        let to_drop = self.destructors
            .get_mut()
            .drain(..)
            .map(|destructor| destructor(state))
            .collect::<Vec<_>>();
        drop(to_drop);
    }
}

static FUNCTION_METATABLE_REGISTRY_KEY: u8 = 0;
