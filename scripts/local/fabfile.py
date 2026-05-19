from fabric import task

@task
def local(ctx):
    '''Runs benchmarks locally'''
    print("Testing")