//# publish
module 0x42::m {

    struct S has copy, drop { t: u64 }

    public fun make_s(t: u64): S {
        S { t }
    }

    public fun t0(a: S, b: S): bool{
       a == b
    }

    public fun t1(a: &S, b: S): bool{
       a == b
    }

    public fun t2(a: S, b: &S): bool{
       a == b
    }

    public fun t3(a: &mut S, b: S): bool{
       a == b
    }

    public fun t4(a: S, b: &mut S): bool{
       a == b
    }

    public fun t5(a: &S, b: &S): bool{
       a == b
    }

    public fun t6(a: &mut S, b: &S): bool{
       a == b
    }

    public fun t7(a: &S, b: &mut S): bool{
       a == b
    }

    public fun t8(a: &mut S, b: &mut S): bool{
       a == b
    }

    public fun t9(): bool{
       0 == &0
    }

    public fun t10(): bool{
       &0 == &0
    }

    public fun t11(): bool{
        let a = 0;
        let b = &mut 0;
        let c = &0;
        a == b && b == c && a == c
    }
}

//# run
module 0x42::main {

    fun main() {
        let s_val = 0x42::m::make_s(42);
        let s_ref = &(0x42::m::make_s(42));
        let s_mut = &mut (0x42::m::make_s(42));

        assert!(0x42::m::t0(s_val, s_val), 0);
        assert!(0x42::m::t1(s_ref, s_val), 0);
        assert!(0x42::m::t2(s_val, s_ref), 0);
        assert!(0x42::m::t3(s_mut, s_val), 0);
        assert!(0x42::m::t4(s_val, s_mut), 0);
        assert!(0x42::m::t5(s_ref, s_ref), 0);
        assert!(0x42::m::t6(s_mut, s_ref), 0);
        assert!(0x42::m::t7(s_ref, s_mut), 0);
        // can't double-borrow the mut here
        // assert!(0x42::m::t8(s_mut, s_mut), 0);

        let s2_val = 0x42::m::make_s(2);
        let s2_ref = &(0x42::m::make_s(2));
        let s2_mut = &mut (0x42::m::make_s(2));

        assert!(!0x42::m::t0(s_val, s2_val), 0);
        assert!(!0x42::m::t1(s_ref, s2_val), 0);
        assert!(!0x42::m::t2(s_val, s2_ref), 0);
        assert!(!0x42::m::t3(s_mut, s2_val), 0);
        assert!(!0x42::m::t4(s_val, s2_mut), 0);
        assert!(!0x42::m::t5(s_ref, s2_ref), 0);
        assert!(!0x42::m::t6(s_mut, s2_ref), 0);
        assert!(!0x42::m::t7(s_ref, s2_mut), 0);
        assert!(!0x42::m::t8(s_mut, s2_mut), 0);

        assert!(0x42::m::t9(), 0);
        assert!(0x42::m::t10(), 0);
        assert!(0x42::m::t11(), 0);
    }
}
