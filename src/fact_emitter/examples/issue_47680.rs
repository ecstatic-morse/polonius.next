use super::*;
use insta::assert_display_snapshot;

#[test]
// Port of /polonius.next/tests/issue-47680/program.txt
fn issue_47680() {
    let program = "
        let thing: Thing;
        let temp: &'temp mut Thing;
        let t0: &'t0 mut Thing;
        let v: &'v mut Thing;

        bb0: {
            temp = &'L_Thing mut thing;
            goto bb1;
        }

        bb1: {
            t0 = &'L_*temp mut *temp;
            v = MaybeNext(move t0);
            goto bb2, bb3;
        }

        bb2: {
            temp = move v;
            goto bb4;
        }

        bb3: {
            goto bb4;
        }

        bb4: {
            goto bb1;
        }
    ";

    // Notes about the current output:
    // - node b: missing subset because of the deref
    // - node c: missing subset between the arguments, the fn signatures lack lifetime bounds
    // - node d: missing clear origin of a loan of the deref

    assert_display_snapshot!(expect_facts(program), @r###"
    a: "temp = &'L_Thing mut thing" {
        invalidate_origin('L_Thing)
        clear_origin('temp)
        clear_origin('L_Thing)
        introduce_subset('L_Thing, 'temp)
        goto b
    }

    b: "t0 = &'L_*temp mut *temp" {
        access_origin('temp)
        invalidate_origin('L_*temp)
        clear_origin('t0)
        clear_origin('L_*temp)
        introduce_subset('L_*temp, 't0)
        goto c
    }

    c: "v = MaybeNext(move t0)" {
        access_origin('t0)
        clear_origin('v)
        goto d e
    }

    d: "temp = move v" {
        access_origin('v)
        clear_origin('temp)
        introduce_subset('v, 'temp)
        goto f
    }

    e: "(pass)" {
        goto f
    }

    f: "(pass)" {
        goto b
    }
    "###);
}
