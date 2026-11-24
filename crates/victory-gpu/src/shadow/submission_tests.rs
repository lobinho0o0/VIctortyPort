use super::*;

#[test]
fn repeated_gpu_writes_keep_their_first_copy_back_order() {
    let Some(host) = host() else { return };
    let mut title = [0u8; 3];
    let start = title.as_mut_ptr() as usize;
    let make_shadow = || {
        let len = title.len() as u64;
        let (buffer, data) = host.shared_buffer(len).unwrap();
        // These lists have no GPU commands. The deferred work below writes the mapped bytes.
        Arc::new(Shadow::new(buffer, data, 0, start as u64, len))
    };
    let a = make_shadow();
    let b = make_shadow();
    let first = host.new_list().unwrap();
    first.defer_refresh(&a, start, 3);
    first.defer_refresh(&b, start, 3);
    first.defer_gpu_write(&a, start, 2);
    first.defer_gpu_write(&a, start, 2);
    first.defer_gpu_write(&a, start, 1);
    first.close();

    let second = host.new_list().unwrap();
    second.defer_gpu_write(&b, start, 2);
    second.defer(move || {
        // SAFETY: the title array lives through both synchronous submissions.
        unsafe { (start as *mut u8).add(2).write(33) };
    });
    second.defer_refresh(&a, start + 2, 1);
    second.defer_gpu_write(&a, start, 2);
    second.defer_host(move || {
        assert_eq!(
            *a.pending.lock().unwrap(),
            [(start, start + 2), (start, start + 1)]
        );
        assert_eq!(*b.pending.lock().unwrap(), [(start, start + 2)]);
        // SAFETY: both shadows own three mapped bytes, used by this submitting thread.
        unsafe {
            assert_eq!(
                a.data.add(2).read(),
                33,
                "a duplicate must still flush refreshes"
            );
            a.data.write(a.data.read() + 11);
            b.data.write(b.data.read() + 22);
        }
    });
    second.close();

    for expected in [22, 44] {
        host.execute(&[&first, &second]);
        assert_eq!(
            title,
            [expected, 0, 33],
            "the second shadow writes back last"
        );
    }
}
