// En un Windows limpio no hay runtime de VC++ (VCRUNTIME140.dll): sin esto el
// exe falla con 0xC0000135 al arrancar. static_vcruntime enlaza vcruntime de
// forma estatica y deja el UCRT dinamico, que forma parte de Windows 10+.
fn main() {
    static_vcruntime::metabuild();
}
