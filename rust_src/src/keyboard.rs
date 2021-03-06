//! keyboard

use remacs_macros::lisp_fn;

use crate::{
    buffers::current_buffer,
    eval::{record_unwind_protect, unbind_to},
    frames::{selected_frame, window_frame_live_or_selected_with_action},
    lisp::defsubr,
    lisp::LispObject,
    lists::{LispCons, LispConsCircularChecks, LispConsEndChecks},
    numbers::IsLispNatnum,
    remacs_sys::{
        command_loop_level, glyph_row_area, interrupt_input_blocked, minibuf_level,
        recursive_edit_1, recursive_edit_unwind, update_mode_lines,
    },
    remacs_sys::{
        make_lispy_position, temporarily_switch_to_single_kboard, window_box_left_offset,
    },
    remacs_sys::{Fpos_visible_in_window_p, Fthrow},
    remacs_sys::{Qexit, Qheader_line, Qhelp_echo, Qmode_line, Qnil, Qt, Qvertical_line},
    threads::c_specpdl_index,
    windows::{selected_window, LispWindowOrSelected},
};

/// Return position information for buffer position POS in WINDOW.
/// POS defaults to point in WINDOW; WINDOW defaults to the selected window.
///
/// Return nil if POS is not visible in WINDOW.  Otherwise,
/// the return value is similar to that returned by `event-start' for
/// a mouse click at the upper left corner of the glyph corresponding
/// to POS:
///    (WINDOW AREA-OR-POS (X . Y) TIMESTAMP OBJECT POS (COL . ROW)
///     IMAGE (DX . DY) (WIDTH . HEIGHT))
/// The `posn-' functions access elements of such lists.
#[lisp_fn(min = "0")]
pub fn posn_at_point(pos: LispObject, window: LispWindowOrSelected) -> LispObject {
    let window: LispObject = window.into();

    let tem = unsafe { Fpos_visible_in_window_p(pos, window, Qt) };
    if tem.is_nil() {
        return Qnil;
    }

    let mut it = tem.iter_cars(LispConsEndChecks::off, LispConsCircularChecks::off);
    let x = it.next().map_or(0, |coord| coord.as_fixnum_or_error());
    let mut y = it.next().map_or(0, |coord| coord.as_fixnum_or_error());

    // Point invisible due to hscrolling?  X can be -1 when a
    // newline in a R2L line overflows into the left fringe.
    if x < -1 {
        return Qnil;
    }
    let aux_info = it.rest();
    if aux_info.is_not_nil() && y < 0 {
        let rtop = it.next().map_or(0, |v| v.as_fixnum_or_error());

        y += rtop;
    }

    posn_at_x_y(x.into(), y.into(), window, Qnil)
}

/// Return position information for pixel coordinates X and Y.
/// By default, X and Y are relative to text area of the selected window.
/// Optional third arg FRAME-OR-WINDOW non-nil specifies frame or window.
/// If optional fourth arg WHOLE is non-nil, X is relative to the left
/// edge of the window.
///
/// The return value is similar to a mouse click position:
///    (WINDOW AREA-OR-POS (X . Y) TIMESTAMP OBJECT POS (COL . ROW)
///     IMAGE (DX . DY) (WIDTH . HEIGHT))
/// The `posn-' functions access elements of such lists.
#[lisp_fn(min = "2")]
pub fn posn_at_x_y(
    objx: LispObject,
    objy: LispObject,
    frame_or_window: LispObject,
    whole: LispObject,
) -> LispObject {
    let x = objx.as_fixnum_or_error();
    if x != -1 {
        x.check_natnum();
    }
    let mut x = x as i32;
    let mut y = objy.as_natnum_or_error() as i32;

    let mut frame = window_frame_live_or_selected_with_action(frame_or_window, |mut w| {
        x += w.left_edge_x();

        if whole.is_nil() {
            x += unsafe { window_box_left_offset(w.as_mut(), glyph_row_area::TEXT_AREA) };
        }

        y = w.frame_pixel_y(y);
    });

    unsafe { make_lispy_position(frame.as_mut(), x.into(), y.into(), 0) }
}

/// Return true if EVENT is a list whose elements are all integers or symbols.
/// Such a list is not valid as an event,
/// but it can be a Lucid-style event type list.
pub fn lucid_event_type_list_p(event: Option<LispCons>) -> bool {
    event.map_or(false, |event| {
        let first = event.car();
        if first.eq(Qhelp_echo)
            || first.eq(Qvertical_line)
            || first.eq(Qmode_line)
            || first.eq(Qheader_line)
        {
            return false;
        }

        let mut it = event.iter_cars(LispConsEndChecks::off, LispConsCircularChecks::off);

        if !it.all(|elt| elt.is_fixnum() || elt.is_symbol()) {
            return false;
        }

        it.rest().is_nil()
    })
}

pub fn quit_recursive_edit(val: bool) -> ! {
    unsafe {
        if command_loop_level > 0 || minibuf_level > 0 {
            Fthrow(Qexit, val.into());
        }

        user_error!("No recursive edit is in progress");
    }
}

/// Exit from the innermost recursive edit or minibuffer.
#[lisp_fn(intspec = "")]
pub fn exit_recursive_edit() -> ! {
    quit_recursive_edit(false);
}

/// Abort the command that requested this recursive edit or minibuffer input.
#[lisp_fn(intspec = "")]
pub fn abort_recursive_edit() -> ! {
    quit_recursive_edit(true);
}

/// Invoke the editor command loop recursively.
/// To get out of the recursive edit, a command can throw to `exit' -- for
/// instance (throw \\='exit nil).
/// If you throw a value other than t, `recursive-edit' returns normally
/// to the function that called it.  Throwing a t value causes
/// `recursive-edit' to quit, so that control returns to the command loop
/// one level up.
///
/// This function is called by the editor initialization to begin editing.
#[lisp_fn(intspec = "")]
pub fn recursive_edit() {
    let count = c_specpdl_index();

    // If we enter while input is blocked, don't lock up here.
    // This may happen through the debugger during redisplay.
    if unsafe { interrupt_input_blocked } > 0 {
        return;
    }

    let buf = if unsafe { command_loop_level >= 0 }
        && selected_window()
            .as_window()
            .map_or(true, |w| !w.contents.eq(current_buffer()))
    {
        current_buffer()
    } else {
        Qnil
    };

    // Don't do anything interesting between the increment and the
    // record_unwind_protect!  Otherwise, we could get distracted and
    // never decrement the counter again.
    unsafe {
        command_loop_level += 1;
        update_mode_lines = 17;
        record_unwind_protect(Some(recursive_edit_unwind), buf);

        if command_loop_level > 0 {
            temporarily_switch_to_single_kboard(selected_frame().as_mut());
        }

        recursive_edit_1();
        unbind_to(count, Qnil);
    }
}

#[no_mangle]
pub extern "C" fn rust_syms_of_keyboard() {
    /// The last command executed.
    /// Normally a symbol with a function definition, but can be whatever was found
    /// in the keymap, or whatever the variable `this-command' was set to by that
    /// command.
    ///
    /// The value `mode-exit' is special; it means that the previous command
    /// read an event that told it to exit, and it did so and unread that event.
    /// In other words, the present command is the event that made the previous
    /// command exit.
    ///
    /// The value `kill-region' is special; it means that the previous command
    /// was a kill command.
    ///
    /// `last-command' has a separate binding for each terminal device.
    /// See Info node `(elisp)Multiple Terminals'.
    defvar_kboard!(Vlast_command_, "last-command");

    /// Same as `last-command', but never altered by Lisp code.
    /// Taken from the previous value of `real-this-command'.
    defvar_kboard!(Vreal_last_command_, "real-last-command");

    /// Last command that may be repeated.
    /// The last command executed that was not bound to an input event.
    /// This is the command `repeat' will try to repeat.
    /// Taken from a previous value of `real-this-command'.  */
    defvar_kboard!(Vlast_repeatable_command_, "last-repeatable-command");
}

include!(concat!(env!("OUT_DIR"), "/keyboard_exports.rs"));
