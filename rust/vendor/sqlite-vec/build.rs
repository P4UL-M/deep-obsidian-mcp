fn main() {
    let mut build = cc::Build::new();
    build.file("sqlite-vec.c");
    build.define("SQLITE_CORE", None);
    build.define("SQLITE_VEC_ENABLE_DISKANN", Some("0"));
    build.define("SQLITE_VEC_ENABLE_RESCORE", Some("0"));
    build.define("SQLITE_VEC_EXPERIMENTAL_IVF_ENABLE", Some("0"));
    build.flag("-DSQLITE_VEC_ENABLE_DISKANN=0");
    build.flag("-DSQLITE_VEC_ENABLE_RESCORE=0");
    build.flag("-DSQLITE_VEC_EXPERIMENTAL_IVF_ENABLE=0");
    build.compile("sqlite_vec0");
}
