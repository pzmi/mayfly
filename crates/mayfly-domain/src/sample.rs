//! One valid value of each validated value type, for tools that need a value without a source.

/// A valid value of a validated value type: one that deserialises only through its parser.
///
/// The event shape tracer (`mayfly_control_domain::shape`) offers each implementor's text to a
/// string leaf it cannot otherwise satisfy, so a payload keyed or led by the type (a map from
/// `HostId`, a tuple that starts with an `OwnedVmName`) is traced past it; a new validated type
/// carried by events implements this and joins the tracer's list (ADR 0018). Each sample
/// deserialises from its own text: pinned by
/// `tests/kernel.rs::should_decode_its_own_text_when_a_sample_is_serialised`.
pub trait Sample {
    /// The sample value, the same on every call.
    fn sample() -> Self;
}
