module a::m {

    struct S has drop { }

    fun t0(a: S, b: S): bool{
       a == b
    }

    fun t1(a: &S, b: S): bool{
       a == b
    }

    fun t2(a: S, b: &S): bool{
       a == b
    }

    fun t3(a: &mut S, b: S): bool{
       a == b
    }

    fun t4(a: S, b: &mut S): bool{
       a == b
    }

    fun t5(a: &S, b: &S): bool{
       a == b
    }

    fun t6(a: &mut S, b: &S): bool{
       a == b
    }

    fun t7(a: &S, b: &mut S): bool{
       a == b
    }

    fun t8(a: &mut S, b: &mut S): bool{
       a == b
    }

    fun t9(): bool{
       0 == &0
    }

    fun t10(): bool{
       &0 == &0
    }

    fun t11(): bool{
        let a = 0;
        let b = &mut 0;
        let c = &0;
        a == b && b == c
    }

}
