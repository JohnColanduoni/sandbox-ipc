
macro_rules! passthrough_debug {
    ($t:ty => $field:ident) => {
        impl ::std::fmt::Debug for $t {
            fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
                ::std::fmt::Debug::fmt(&self.$field, f)
            }
        }
    }
}