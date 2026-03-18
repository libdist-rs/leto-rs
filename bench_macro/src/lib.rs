use proc_macro::TokenStream;

#[proc_macro_attribute]
pub fn time_this_function(_attr: TokenStream, item: TokenStream) -> TokenStream {
    // TODO: Parse _attr as a name of the benchmark flag FLAG
    // TODO: Transform visibility fn name(...) -> Return type { .. } -> visibility fn name1(...) -> Return type {  }
    item
} 